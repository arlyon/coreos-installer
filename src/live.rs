// Copyright 2019 CoreOS, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{bail, Context, Result};
use bytes::Buf;
use lazy_static::lazy_static;
use nix::unistd::isatty;
use openat_ext::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryInto;
use std::fs::{create_dir_all, read, write, File, OpenOptions};
use std::io::{self, copy, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::iter::repeat;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::cmdline::*;
use crate::io::*;
use crate::iso9660::{self, IsoFs};
use crate::miniso;

const INITRD_IGNITION_PATH: &str = "config.ign";
const INITRD_NETWORK_DIR: &str = "etc/coreos-firstboot-network";
const INITRD_LIVE_STAMP_PATH: &str = "etc/coreos-live-initramfs";
const INITRD_FEATURES_PATH: &str = "etc/coreos/features.json";
const COREOS_INITRD_EMBED_PATH: &str = "IMAGES/IGNITION.IMG";
const COREOS_INITRD_HEADER_SIZE: u64 = 24;
const COREOS_KARG_EMBED_AREA_HEADER_MAGIC: &[u8] = b"coreKarg";
const COREOS_KARG_EMBED_AREA_HEADER_SIZE: u64 = 72;
const COREOS_KARG_EMBED_AREA_HEADER_MAX_OFFSETS: usize = 6;
const COREOS_KARG_EMBED_AREA_MAX_SIZE: usize = 2048;
const COREOS_KARG_EMBED_INFO_PATH: &str = "COREOS/KARGS.JSO";
const COREOS_ISO_FEATURES_PATH: &str = "COREOS/FEATURES.JSO";
const COREOS_ISO_PXEBOOT_DIR: &str = "IMAGES/PXEBOOT";
const COREOS_ISO_ROOTFS_IMG: &str = "IMAGES/PXEBOOT/ROOTFS.IMG";
const COREOS_ISO_MINISO_FILE: &str = "COREOS/MINISO.DAT";

lazy_static! {
    static ref INITRD_IGNITION_GLOB: GlobMatcher =
        GlobMatcher::new(&[INITRD_IGNITION_PATH]).unwrap();
    static ref INITRD_NETWORK_GLOB: GlobMatcher =
        GlobMatcher::new(&[&format!("{}/*", INITRD_NETWORK_DIR)]).unwrap();
}

pub fn iso_embed(config: IsoEmbedConfig) -> Result<()> {
    eprintln!("`iso embed` is deprecated; use `iso ignition embed`.  Continuing.");
    iso_ignition_embed(IsoIgnitionEmbedConfig {
        force: config.force,
        ignition_file: config.config,
        output: config.output,
        input: config.input,
    })
}

pub fn iso_show(config: IsoShowConfig) -> Result<()> {
    eprintln!("`iso show` is deprecated; use `iso ignition show`.  Continuing.");
    iso_ignition_show(IsoIgnitionShowConfig {
        input: config.input,
        header: false,
    })
}

pub fn iso_remove(config: IsoRemoveConfig) -> Result<()> {
    eprintln!("`iso remove` is deprecated; use `iso ignition remove`.  Continuing.");
    iso_ignition_remove(IsoIgnitionRemoveConfig {
        output: config.output,
        input: config.input,
    })
}

pub fn iso_ignition_embed(config: IsoIgnitionEmbedConfig) -> Result<()> {
    let ignition = match &config.ignition_file {
        Some(ignition_path) => {
            read(ignition_path).with_context(|| format!("reading {}", ignition_path))?
        }
        None => {
            let mut data = Vec::new();
            io::stdin()
                .lock()
                .read_to_end(&mut data)
                .context("reading stdin")?;
            data
        }
    };

    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso = IsoConfig::for_file(&mut iso_file)?;

    if !config.force && iso.have_ignition() {
        bail!("This ISO image already has an embedded Ignition config; use -f to force.");
    }

    iso.initrd_mut().add(INITRD_IGNITION_PATH, ignition);

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn iso_ignition_show(config: IsoIgnitionShowConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, None)?;
    let iso = IsoConfig::for_file(&mut iso_file)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if config.header {
        serde_json::to_writer_pretty(&mut out, &iso.initrd)
            .context("failed to serialize header")?;
        out.write_all(b"\n").context("failed to write newline")?;
    } else {
        if !iso.have_ignition() {
            bail!("No embedded Ignition config.");
        }
        out.write_all(
            iso.initrd()
                .get(INITRD_IGNITION_PATH)
                .context("couldn't find Ignition config in archive")?,
        )
        .context("writing output")?;
        out.flush().context("flushing output")?;
    }
    Ok(())
}

pub fn iso_ignition_remove(config: IsoIgnitionRemoveConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso = IsoConfig::for_file(&mut iso_file)?;

    iso.initrd_mut().remove(INITRD_IGNITION_PATH);

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn iso_network_embed(config: IsoNetworkEmbedConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso = IsoConfig::for_file(&mut iso_file)?;

    if !config.force && iso.have_network() {
        bail!("This ISO image already has embedded network settings; use -f to force.");
    }

    iso.remove_network();
    initrd_network_embed(iso.initrd_mut(), &config.keyfile)?;

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn iso_network_extract(config: IsoNetworkExtractConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, None)?;
    let iso = IsoConfig::for_file(&mut iso_file)?;
    initrd_network_extract(iso.initrd(), config.directory.as_ref())
}

pub fn iso_network_remove(config: IsoNetworkRemoveConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso = IsoConfig::for_file(&mut iso_file)?;

    iso.remove_network();

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn pxe_ignition_wrap(config: PxeIgnitionWrapConfig) -> Result<()> {
    if config.output.is_none() {
        verify_stdout_not_tty()?;
    }

    let ignition = match &config.ignition_file {
        Some(ignition_path) => {
            read(ignition_path).with_context(|| format!("reading {}", ignition_path))?
        }
        None => {
            let mut data = Vec::new();
            io::stdin()
                .lock()
                .read_to_end(&mut data)
                .context("reading stdin")?;
            data
        }
    };

    let mut initrd = Initrd::default();
    initrd.add(INITRD_IGNITION_PATH, ignition);

    write_live_pxe(&initrd, config.output.as_ref())
}

pub fn pxe_ignition_unwrap(config: PxeIgnitionUnwrapConfig) -> Result<()> {
    let stdin = io::stdin();
    let mut f: Box<dyn Read> = if let Some(path) = &config.input {
        Box::new(
            OpenOptions::new()
                .read(true)
                .open(path)
                .with_context(|| format!("opening {}", path))?,
        )
    } else {
        Box::new(stdin.lock())
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(
        Initrd::from_reader_filtered(&mut f, &INITRD_IGNITION_GLOB)?
            .get(INITRD_IGNITION_PATH)
            .context("couldn't find Ignition config in archive")?,
    )
    .context("writing output")?;
    out.flush().context("flushing output")?;
    Ok(())
}

pub fn pxe_network_wrap(config: PxeNetworkWrapConfig) -> Result<()> {
    if config.output.is_none() {
        verify_stdout_not_tty()?;
    }

    let mut initrd = Initrd::default();
    initrd_network_embed(&mut initrd, &config.keyfile)?;

    write_live_pxe(&initrd, config.output.as_ref())
}

fn initrd_network_embed(initrd: &mut Initrd, keyfiles: &[String]) -> Result<()> {
    for path in keyfiles {
        let data = read(path).with_context(|| format!("reading {}", path))?;
        let name = filename(path)?;
        let path = format!("{}/{}", INITRD_NETWORK_DIR, name);
        if initrd.get(&path).is_some() {
            bail!("multiple input files named '{}'", name);
        }
        initrd.add(&path, data);
    }
    Ok(())
}

pub fn pxe_network_unwrap(config: PxeNetworkUnwrapConfig) -> Result<()> {
    let stdin = io::stdin();
    let f: Box<dyn Read> = if let Some(path) = &config.input {
        Box::new(
            OpenOptions::new()
                .read(true)
                .open(path)
                .with_context(|| format!("opening {}", path))?,
        )
    } else {
        Box::new(stdin.lock())
    };
    initrd_network_extract(
        &Initrd::from_reader_filtered(f, &INITRD_NETWORK_GLOB)?,
        config.directory.as_ref(),
    )
}

fn initrd_network_extract(initrd: &Initrd, directory: Option<&String>) -> Result<()> {
    let files = initrd.find(&INITRD_NETWORK_GLOB);
    if files.is_empty() {
        bail!("No embedded network settings.");
    }
    if let Some(dir) = directory {
        create_dir_all(&dir)?;
        for (path, contents) in files {
            let path = Path::new(dir).join(filename(path)?);
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?
                .write_all(contents)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("{}", path.display());
        }
    } else {
        for (i, (path, contents)) in files.iter().enumerate() {
            if i > 0 {
                println!();
            }
            println!("########## {} ##########", filename(path)?);
            io::stdout()
                .lock()
                .write_all(contents)
                .context("writing network settings to stdout")?;
        }
    }
    Ok(())
}

pub fn iso_kargs_modify(config: IsoKargsModifyConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso = IsoConfig::for_file(&mut iso_file)?;

    let kargs = KargsEditor::new()
        .append(&config.append)
        .replace(&config.replace)
        .delete(&config.delete)
        .apply_to(iso.kargs()?)?;
    iso.set_kargs(&kargs)?;

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn iso_kargs_reset(config: IsoKargsResetConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso = IsoConfig::for_file(&mut iso_file)?;

    iso.set_kargs(&iso.kargs_default()?.to_string())?;

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn iso_kargs_show(config: IsoKargsShowConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, None)?;
    let iso = IsoConfig::for_file(&mut iso_file)?;
    if config.header {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        serde_json::to_writer_pretty(&mut out, &iso.kargs).context("failed to serialize header")?;
        out.write_all(b"\n").context("failed to write newline")?;
    } else {
        let kargs = if config.default {
            iso.kargs_default()?
        } else {
            iso.kargs()?
        };
        println!("{}", kargs);
    }
    Ok(())
}

pub fn iso_customize(config: IsoCustomizeConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso_fs = IsoFs::from_file(iso_file.try_clone().context("cloning file")?)
        .context("parsing ISO9660 image")?;
    let mut iso = IsoConfig::for_iso(&mut iso_fs)?;

    if !config.force
        && (iso.have_ignition() || iso.have_network() || iso.kargs()? != iso.kargs_default()?)
    {
        bail!("This ISO image is already customized; use -f to force.");
    }

    // read OS features
    let features = match iso_fs.get_path(COREOS_ISO_FEATURES_PATH) {
        Ok(record) => serde_json::from_reader(
            iso_fs
                .read_file(&record.try_into_file()?)
                .context("reading OS features")?,
        )
        .context("parsing OS features")?,
        Err(e) if e.is::<iso9660::NotFound>() => OsFeatures::default(),
        Err(e) => return Err(e).context("looking up OS features"),
    };

    let live = LiveInitrd::from_common(&config.common, features)?;
    *iso.initrd_mut() = live.into_initrd()?;

    let kargs = KargsEditor::new()
        .append(&config.live_karg_append)
        .replace(&config.live_karg_replace)
        .delete(&config.live_karg_delete)
        .apply_to(iso.kargs_default()?)?;
    iso.set_kargs(&kargs)?;

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn iso_reset(config: IsoResetConfig) -> Result<()> {
    let mut iso_file = open_live_iso(&config.input, Some(config.output.as_ref()))?;
    let mut iso = IsoConfig::for_file(&mut iso_file)?;

    *iso.initrd_mut() = Initrd::default();
    iso.set_kargs(&iso.kargs_default()?.to_string())?;

    write_live_iso(&iso, &mut iso_file, config.output.as_ref())
}

pub fn pxe_customize(config: PxeCustomizeConfig) -> Result<()> {
    // open input and set up output
    let mut input = BufReader::with_capacity(
        BUFFER_SIZE,
        OpenOptions::new()
            .read(true)
            .open(&config.input)
            .with_context(|| format!("opening {}", &config.input))?,
    );
    let mut tempfile = match &*config.output {
        "-" => {
            verify_stdout_not_tty()?;
            None
        }
        path => {
            let dir = Path::new(path)
                .parent()
                .with_context(|| format!("no parent directory of {}", path))?;
            let tempfile = tempfile::Builder::new()
                .prefix(".coreos-installer-temp-")
                .tempfile_in(dir)
                .context("creating temporary file")?;
            Some(tempfile)
        }
    };

    // copy and check base initrd
    let filter = GlobMatcher::new(&[
        INITRD_LIVE_STAMP_PATH,
        INITRD_FEATURES_PATH,
        INITRD_IGNITION_PATH,
        &format!("{}/*", INITRD_NETWORK_DIR),
    ])
    .unwrap();
    let base_initrd = match &*config.output {
        "-" => {
            Initrd::from_reader_filtered(TeeReader::new(&mut input, io::stdout().lock()), &filter)
                .context("reading/copying input initrd")?
        }
        _ => Initrd::from_reader_filtered(
            TeeReader::new(&mut input, tempfile.as_mut().unwrap()),
            &filter,
        )
        .context("reading/copying input initrd")?,
    };
    if base_initrd.get(INITRD_LIVE_STAMP_PATH).is_none() {
        bail!("not a CoreOS live initramfs image");
    }
    if base_initrd.get(INITRD_IGNITION_PATH).is_some()
        || !base_initrd.find(&INITRD_NETWORK_GLOB).is_empty()
    {
        bail!("input is already customized");
    }
    let features = match base_initrd.get(INITRD_FEATURES_PATH) {
        Some(json) => serde_json::from_slice::<OsFeatures>(json).context("parsing OS features")?,
        None => OsFeatures::default(),
    };

    let live = LiveInitrd::from_common(&config.common, features)?;
    let initrd = live.into_initrd()?;

    // append customizations to output
    let do_write = |writer: &mut dyn Write| -> Result<()> {
        let mut buf = BufWriter::with_capacity(BUFFER_SIZE, writer);
        buf.write_all(&initrd.to_bytes()?)
            .context("writing initrd")?;
        buf.flush().context("flushing initrd")
    };
    match &*config.output {
        "-" => do_write(&mut io::stdout().lock()),
        path => {
            let mut tempfile = tempfile.unwrap();
            do_write(tempfile.as_file_mut())?;
            tempfile
                .persist_noclobber(&path)
                .map_err(|e| e.error)
                .with_context(|| format!("persisting output file to {}", path))?;
            Ok(())
        }
    }
}

// output_path should be None if not outputting, or Some(output_path_argument)
fn open_live_iso(input_path: &str, output_path: Option<Option<&String>>) -> Result<File> {
    // if output_path is Some(None), we're modifying in place, so we need to
    // open for writing
    OpenOptions::new()
        .read(true)
        .write(matches!(output_path, Some(None)))
        .open(&input_path)
        .with_context(|| format!("opening {}", &input_path))
}

fn write_live_iso(iso: &IsoConfig, input: &mut File, output_path: Option<&String>) -> Result<()> {
    match output_path.map(|v| v.as_str()) {
        None => {
            // open_live_iso() opened input for writing
            iso.write(input)?;
        }
        Some("-") => {
            verify_stdout_not_tty()?;
            iso.stream(input, &mut io::stdout().lock())?;
        }
        Some(output_path) => {
            let output_dir = Path::new(output_path)
                .parent()
                .with_context(|| format!("no parent directory of {}", output_path))?;
            let mut output = tempfile::Builder::new()
                .prefix(".coreos-installer-temp-")
                .tempfile_in(output_dir)
                .context("creating temporary file")?;
            input.seek(SeekFrom::Start(0)).context("seeking input")?;
            input
                .copy_to(output.as_file_mut())
                .context("copying input to temporary file")?;
            iso.write(output.as_file_mut())?;
            output
                .persist_noclobber(&output_path)
                .map_err(|e| e.error)
                .with_context(|| format!("persisting output file to {}", output_path))?;
        }
    }
    Ok(())
}

/// If output_path is None, we write to stdout.  The caller is expected to
/// have called verify_stdout_not_tty() in this case.
fn write_live_pxe(initrd: &Initrd, output_path: Option<&String>) -> Result<()> {
    let initrd = initrd.to_bytes()?;
    match output_path {
        Some(path) => write(path, &initrd).with_context(|| format!("writing {}", path)),
        None => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(&initrd).context("writing output")?;
            out.flush().context("flushing output")
        }
    }
}

struct IsoConfig {
    initrd: InitrdEmbedArea,
    kargs: Option<KargEmbedAreas>,
}

impl IsoConfig {
    pub fn for_file(file: &mut File) -> Result<Self> {
        let mut iso = IsoFs::from_file(file.try_clone().context("cloning file")?)
            .context("parsing ISO9660 image")?;
        IsoConfig::for_iso(&mut iso)
    }

    pub fn for_iso(iso: &mut IsoFs) -> Result<Self> {
        Ok(Self {
            initrd: InitrdEmbedArea::for_iso(iso)?,
            kargs: KargEmbedAreas::for_iso(iso)?,
        })
    }

    pub fn have_ignition(&self) -> bool {
        self.initrd().get(INITRD_IGNITION_PATH).is_some()
    }

    pub fn have_network(&self) -> bool {
        !self.initrd().find(&INITRD_NETWORK_GLOB).is_empty()
    }

    pub fn remove_network(&mut self) {
        let initrd = self.initrd_mut();
        let paths: Vec<String> = initrd
            .find(&INITRD_NETWORK_GLOB)
            .keys()
            .map(|p| p.to_string())
            .collect();
        for path in paths {
            initrd.remove(&path);
        }
    }

    pub fn initrd(&self) -> &Initrd {
        self.initrd.initrd()
    }

    pub fn initrd_mut(&mut self) -> &mut Initrd {
        self.initrd.initrd_mut()
    }

    pub fn kargs(&self) -> Result<&str> {
        Ok(self.unwrap_kargs()?.kargs())
    }

    pub fn kargs_default(&self) -> Result<&str> {
        Ok(self.unwrap_kargs()?.kargs_default())
    }

    pub fn set_kargs(&mut self, kargs: &str) -> Result<()> {
        self.unwrap_kargs_mut()?.set_kargs(kargs)
    }

    fn unwrap_kargs(&self) -> Result<&KargEmbedAreas> {
        self.kargs
            .as_ref()
            .context("No karg embed areas found; old or corrupted CoreOS ISO image.")
    }

    fn unwrap_kargs_mut(&mut self) -> Result<&mut KargEmbedAreas> {
        self.kargs
            .as_mut()
            .context("No karg embed areas found; old or corrupted CoreOS ISO image.")
    }

    pub fn write(&self, file: &mut File) -> Result<()> {
        self.initrd.write(file)?;
        if let Some(kargs) = &self.kargs {
            kargs.write(file)?;
        }
        Ok(())
    }

    pub fn stream(&self, input: &mut File, writer: &mut (impl Write + ?Sized)) -> Result<()> {
        let initrd_region = self.initrd.region()?;
        let mut regions = vec![&initrd_region];
        if let Some(kargs) = &self.kargs {
            regions.extend(kargs.regions.iter())
        }
        regions.stream(input, writer)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Region {
    // sort order is derived from field order
    pub offset: u64,
    pub length: usize,
    #[serde(skip_serializing)]
    pub contents: Vec<u8>,
    #[serde(skip_serializing)]
    pub modified: bool,
}

impl Region {
    pub fn read(file: &mut File, offset: u64, length: usize) -> Result<Self> {
        let mut contents = vec![0; length];
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seeking to offset {}", offset))?;
        file.read_exact(&mut contents)
            .with_context(|| format!("reading {} bytes at {}", length, offset))?;
        Ok(Self {
            offset,
            length,
            contents,
            modified: false,
        })
    }

    pub fn write(&self, file: &mut File) -> Result<()> {
        self.validate()?;
        if self.modified {
            file.seek(SeekFrom::Start(self.offset))
                .with_context(|| format!("seeking to offset {}", self.offset))?;
            file.write_all(&self.contents)
                .with_context(|| format!("writing {} bytes at {}", self.length, self.offset))?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.length != self.contents.len() {
            bail!(
                "expected region contents length {}, found {}",
                self.length,
                self.contents.len()
            );
        }
        Ok(())
    }
}

trait Stream {
    fn stream(&self, input: &mut File, writer: &mut (impl Write + ?Sized)) -> Result<()>;
}

impl Stream for [&Region] {
    fn stream(&self, input: &mut File, writer: &mut (impl Write + ?Sized)) -> Result<()> {
        input.seek(SeekFrom::Start(0)).context("seeking to start")?;

        let mut regions: Vec<&&Region> = self.iter().filter(|r| r.modified).collect();
        regions.sort_unstable();

        let mut buf = [0u8; BUFFER_SIZE];
        let mut cursor: u64 = 0;

        // validate regions
        for region in &regions {
            region.validate()?;
            if region.offset < cursor {
                bail!(
                    "region starting at {} precedes current offset {}",
                    region.offset,
                    cursor
                );
            }
            cursor = region.offset + region.length as u64;
        }

        // write regions
        cursor = 0;
        for region in &regions {
            assert!(region.offset >= cursor);
            copy_exactly_n(input, writer, region.offset - cursor, &mut buf)
                .with_context(|| format!("copying bytes from {} to {}", cursor, region.offset))?;
            writer.write_all(&region.contents).with_context(|| {
                format!(
                    "writing region for {} at offset {}",
                    region.length, region.offset
                )
            })?;
            cursor = input
                .seek(SeekFrom::Current(region.length as i64))
                .with_context(|| format!("seeking region length {}", region.length))?;
        }

        // write the remainder
        let mut write_buf = BufWriter::with_capacity(BUFFER_SIZE, writer);
        copy(
            &mut BufReader::with_capacity(BUFFER_SIZE, input),
            &mut write_buf,
        )
        .context("copying file")?;
        write_buf.flush().context("flushing output")?;
        Ok(())
    }
}

#[derive(Serialize)]
struct KargEmbedAreas {
    length: usize,
    default: String,

    #[serde(rename = "kargs")]
    regions: Vec<Region>,
    #[serde(skip_serializing)]
    args: String,
}

#[derive(Deserialize, Serialize)]
struct KargEmbedInfo {
    default: String,
    files: Vec<KargEmbedLocation>,
    size: usize,
}

#[derive(Deserialize, Serialize)]
struct KargEmbedLocation {
    path: String,
    offset: u64,
}

impl KargEmbedInfo {
    // Returns Ok(None) if `kargs.json` doesn't exist.
    pub fn for_iso(iso: &mut IsoFs) -> Result<Option<Self>> {
        let iso_file = match iso.get_path(COREOS_KARG_EMBED_INFO_PATH) {
            Ok(record) => record.try_into_file()?,
            // old ISO without info JSON
            Err(e) if e.is::<iso9660::NotFound>() => return Ok(None),
            Err(e) => return Err(e),
        };
        let info: KargEmbedInfo = serde_json::from_reader(
            iso.read_file(&iso_file)
                .context("reading kargs embed area info")?,
        )
        .context("decoding kargs embed area info")?;
        Ok(Some(info))
    }

    pub fn update_iso(&self, iso: &mut IsoFs) -> Result<()> {
        let iso_file = iso.get_path(COREOS_KARG_EMBED_INFO_PATH)?.try_into_file()?;
        let mut w = iso.overwrite_file(&iso_file)?;
        let new_json = serde_json::to_string_pretty(&self).context("serializing object")?;
        if new_json.len() > iso_file.length as usize {
            // This really shouldn't happen. It's only used by the miniso stuff, and there we
            // strictly *remove* kargs from the default set.
            bail!(
                "New version of {} does not fit in space ({} vs {})",
                COREOS_KARG_EMBED_INFO_PATH,
                new_json.len(),
                iso_file.length,
            );
        }

        let mut contents = vec![b' '; iso_file.length as usize];
        contents[..new_json.len()].copy_from_slice(new_json.as_bytes());
        w.write_all(&contents)
            .with_context(|| format!("failed to update {}", COREOS_KARG_EMBED_INFO_PATH))?;
        w.flush().context("flushing ISO")?;
        Ok(())
    }
}

impl KargEmbedAreas {
    // Return Ok(None) if no kargs embed areas exist.
    pub fn for_iso(iso: &mut IsoFs) -> Result<Option<Self>> {
        let info = match KargEmbedInfo::for_iso(iso)? {
            Some(info) => info,
            None => return Self::for_file_via_system_area(iso.as_file()?),
        };

        // sanity-check size against a reasonable limit
        if info.size > COREOS_KARG_EMBED_AREA_MAX_SIZE {
            bail!(
                "karg embed area size larger than {} (found {})",
                COREOS_KARG_EMBED_AREA_MAX_SIZE,
                info.size
            );
        }
        if info.default.len() > info.size {
            bail!(
                "default kargs size {} larger than embed areas ({})",
                info.default.len(),
                info.size
            );
        }

        // writable regions
        let mut regions = Vec::new();
        for loc in info.files {
            let iso_file = iso
                .get_path(&loc.path.to_uppercase())
                .with_context(|| format!("looking up '{}'", loc.path))?
                .try_into_file()?;
            // we rely on Region::read() to verify that the offset/length
            // pair is in bounds
            regions.push(
                Region::read(
                    iso.as_file()?,
                    iso_file.address.as_offset() + loc.offset,
                    info.size,
                )
                .context("reading kargs embed area")?,
            );
        }
        regions.sort_unstable_by_key(|r| r.offset);

        Some(Self::build(info.size, info.default, regions)).transpose()
    }

    fn for_file_via_system_area(file: &mut File) -> Result<Option<Self>> {
        // The ISO 9660 System Area is 32 KiB. Karg embed area information is located in the 72 bytes
        // before the initrd embed area (see EmbedArea below):
        // 8 bytes: magic string "coreKarg"
        // 8 bytes little-endian: length of karg embed areas
        // 8 bytes little-endian: offset to default kargs
        // 8 bytes little-endian x 6: offsets to karg embed areas
        let region = Region::read(
            file,
            32768 - COREOS_INITRD_HEADER_SIZE - COREOS_KARG_EMBED_AREA_HEADER_SIZE,
            COREOS_KARG_EMBED_AREA_HEADER_SIZE as usize,
        )
        .context("reading karg embed header")?;
        let mut header = &region.contents[..];
        // magic number
        if header.copy_to_bytes(8) != COREOS_KARG_EMBED_AREA_HEADER_MAGIC {
            return Ok(None);
        }
        // length
        let length: usize = header
            .get_u64_le()
            .try_into()
            .context("karg embed area length too large to allocate")?;
        // sanity-check against a reasonable limit
        if length > COREOS_KARG_EMBED_AREA_MAX_SIZE {
            bail!(
                "karg embed area length larger than {} (found {})",
                COREOS_KARG_EMBED_AREA_MAX_SIZE,
                length
            );
        }

        // we rely on Region::read() to verify that offset/length pairs are
        // in bounds

        // default kargs
        let offset = header.get_u64_le();
        let default_region = Region::read(file, offset, length).context("reading default kargs")?;
        let default = Self::parse(&default_region)?;

        // writable regions
        let mut regions = Vec::new();
        while regions.len() < COREOS_KARG_EMBED_AREA_HEADER_MAX_OFFSETS {
            let offset = header.get_u64_le();
            if offset == 0 {
                break;
            }
            regions.push(Region::read(file, offset, length).context("reading kargs embed area")?);
        }

        Some(Self::build(length, default, regions)).transpose()
    }

    fn build(length: usize, default: String, regions: Vec<Region>) -> Result<Self> {
        // we expect at least one region
        if regions.is_empty() {
            bail!("No karg embed areas found; corrupted CoreOS ISO image.");
        }

        // parse kargs and verify that all the offsets have the same arguments
        let args = Self::parse(&regions[0])?;
        for region in regions.iter().skip(1) {
            let current_args = Self::parse(region)?;
            if current_args != args {
                bail!(
                    "kargs don't match at all offsets! (expected '{}', but offset {} has: '{}')",
                    args,
                    region.offset,
                    current_args
                );
            }
        }

        Ok(Self {
            length,
            default,
            regions,
            args,
        })
    }

    fn parse(region: &Region) -> Result<String> {
        Ok(String::from_utf8(region.contents.clone())
            .context("invalid UTF-8 in karg area")?
            .trim_end_matches('#')
            .trim()
            .into())
    }

    pub fn kargs_default(&self) -> &str {
        &self.default
    }

    pub fn kargs(&self) -> &str {
        &self.args
    }

    pub fn set_kargs(&mut self, kargs: &str) -> Result<()> {
        let unformatted = kargs.trim();
        let formatted = unformatted.to_string() + "\n";
        if formatted.len() > self.length {
            bail!(
                "kargs too large for area: {} vs {}",
                formatted.len(),
                self.length
            );
        }
        let mut contents = vec![b'#'; self.length];
        contents[..formatted.len()].copy_from_slice(formatted.as_bytes());
        for region in &mut self.regions {
            region.contents = contents.clone();
            region.modified = true;
        }
        self.args = unformatted.to_string();
        Ok(())
    }

    pub fn write(&self, file: &mut File) -> Result<()> {
        for region in &self.regions {
            region.write(file)?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct InitrdEmbedArea {
    // region.contents is kept zero-length; region is cloned upon writing
    #[serde(flatten)]
    region: Region,
    #[serde(skip)]
    initrd: Initrd,
}

impl InitrdEmbedArea {
    pub fn for_iso(iso: &mut IsoFs) -> Result<Self> {
        let f = iso
            .get_path(COREOS_INITRD_EMBED_PATH)
            .context("finding initrd embed area")?
            .try_into_file()?;
        // read (checks offset/length as a side effect)
        let mut region = Region::read(iso.as_file()?, f.address.as_offset(), f.length as usize)
            .context("reading initrd embed area")?;
        let initrd = if region.contents.iter().any(|v| *v != 0) {
            Initrd::from_reader(&*region.contents).context("decoding initrd embed area")?
        } else {
            Initrd::default()
        };
        // free up the memory; we won't need it
        region.contents = Vec::new();
        Ok(Self { region, initrd })
    }

    pub fn initrd(&self) -> &Initrd {
        &self.initrd
    }

    pub fn initrd_mut(&mut self) -> &mut Initrd {
        self.region.modified = true;
        &mut self.initrd
    }

    pub fn write(&self, file: &mut File) -> Result<()> {
        self.region()?.write(file)
    }

    pub fn region(&self) -> Result<Region> {
        // taking &mut self for the deferred update to self.region would
        // require too many other methods to do the same, so clone the
        // region and return that
        let mut region = self.region.clone();
        let capacity = region.length;
        let mut data = if !self.initrd().is_empty() {
            self.initrd().to_bytes()?
        } else {
            Vec::new()
        };
        if data.len() > capacity {
            bail!(
                "Compressed initramfs is too large: {} > {}",
                data.len(),
                capacity
            )
        }
        data.extend(repeat(0).take(capacity - data.len()));
        region.contents = data;
        Ok(region)
    }
}

/// CoreOS feature flags in /etc/coreos/features.json in the live initramfs
/// and /coreos/features.json in the live ISO.  Written by
/// cosa buildextend-live.
#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct OsFeatures {
    /// Installer reads config files from /etc/coreos/installer.d
    installer_config: bool,
    /// Live initrd reads NM keyfiles from /etc/coreos-firstboot-network
    live_initrd_network: bool,
}

#[derive(Default)]
struct LiveInitrd {
    /// OS features
    features: OsFeatures,

    /// The initrd for the live system
    initrd: Initrd,
    /// The Ignition config for the live system
    live: Ignition,
    /// The Ignition config for the destination system
    dest: Option<Ignition>,
    /// User-supplied Ignition configs for the dest system, which might be
    /// merged into the dest config or might become the dest config
    user_dest: Vec<ignition_config::Config>,
    /// The coreos-installer config for our own parameters, excluding custom
    /// configs supplied by the user
    installer: Option<InstallConfig>,
    /// Have the installer copy network configs, if we are running it
    installer_copy_network: bool,
    /// Ignition CAs for the dest system, if it has an Ignition config
    dest_ca: Vec<Vec<u8>>,

    /// Prefix for installer config filenames
    installer_serial: u32,
}

impl LiveInitrd {
    fn from_common(common: &CommonCustomizeConfig, features: OsFeatures) -> Result<Self> {
        let mut conf = Self {
            features,
            ..Default::default()
        };

        for path in &common.dest_ignition {
            conf.dest_ignition(path)?;
        }
        if let Some(path) = &common.dest_device {
            conf.dest_device(path)?;
        }
        for arg in &common.dest_karg_append {
            conf.dest_karg_append(arg);
        }
        for arg in &common.dest_karg_delete {
            conf.dest_karg_delete(arg);
        }
        for path in &common.network_keyfile {
            conf.network_keyfile(path)?;
        }
        for path in &common.ignition_ca {
            conf.ignition_ca(path)?;
        }
        for path in &common.pre_install {
            conf.pre_install(path)?;
        }
        for path in &common.post_install {
            conf.post_install(path)?;
        }
        for path in &common.installer_config {
            conf.installer_config(path)?;
        }
        for path in &common.live_ignition {
            conf.live_config(path)?;
        }

        Ok(conf)
    }

    fn dest_ignition(&mut self, path: &str) -> Result<()> {
        let data = read(path).with_context(|| format!("reading {}", path))?;
        let (config, warnings) = ignition_config::Config::parse_slice(&data)
            .with_context(|| format!("parsing Ignition config {}", path))?;
        for warning in warnings {
            eprintln!("Warning parsing {}: {}", path, warning);
        }
        self.user_dest.push(config);
        Ok(())
    }

    fn dest_device(&mut self, device: &str) -> Result<()> {
        eprintln!(
            "Warning: boot media will overwrite {} without confirmation.",
            device
        );
        self.installer
            .get_or_insert_with(Default::default)
            .dest_device = Some(device.into());
        Ok(())
    }

    fn dest_karg_append(&mut self, arg: &str) {
        self.installer
            .get_or_insert_with(Default::default)
            .append_karg
            .push(arg.into());
    }

    fn dest_karg_delete(&mut self, arg: &str) {
        self.installer
            .get_or_insert_with(Default::default)
            .delete_karg
            .push(arg.into());
    }

    fn network_keyfile(&mut self, path: &str) -> Result<()> {
        if !self.features.live_initrd_network {
            bail!("This OS image does not support customizing network settings.");
        }
        let data = read(path).with_context(|| format!("reading {}", path))?;
        let name = filename(path)?;
        let path = format!("{}/{}", INITRD_NETWORK_DIR, name);
        if self.initrd.get(&path).is_some() {
            bail!("config already specifies keyfile {}", name);
        }
        self.initrd.add(&path, data);
        self.installer_copy_network = true;
        Ok(())
    }

    fn ignition_ca(&mut self, path: &str) -> Result<()> {
        let data = read(path).with_context(|| format!("reading {}", path))?;
        self.live.add_ca(&data)?;
        self.dest_ca.push(data);
        Ok(())
    }

    fn pre_install(&mut self, path: &str) -> Result<()> {
        self.install_hook(
            path,
            "pre",
            "After=coreos-installer-pre.target\nBefore=coreos-installer.service",
            "coreos-installer.service",
        )
    }

    fn post_install(&mut self, path: &str) -> Result<()> {
        self.install_hook(
            path,
            "post",
            "After=coreos-installer.service\nBefore=coreos-installer.target",
            "coreos-installer.target",
        )
    }

    fn install_hook(
        &mut self,
        path: &str,
        typ: &str,
        deps: &str,
        install_target: &str,
    ) -> Result<()> {
        let data = read(path).with_context(|| format!("reading {}", path))?;
        let name = filename(path)?;
        self.live.add_file(
            format!("/usr/local/bin/{}-install-{}", typ, name),
            &data,
            0o700,
        )?;
        self.live.add_unit(
            format!("{}-install-{}.service", typ, name),
            format!(
                "# Generated by coreos-installer {{iso|pxe}} customize

[Unit]
Description={typ_title}-Install Script ({name})
Documentation=https://coreos.github.io/coreos-installer/customizing-install/
{deps}

[Service]
Type=oneshot
ExecStart=/usr/local/bin/{typ}-install-{name}
RemainAfterExit=true
StandardOutput=kmsg+console
StandardError=kmsg+console

[Install]
RequiredBy={install_target}",
                name = name,
                typ = typ,
                typ_title = format!("{}{}", typ[..1].to_uppercase(), &typ[1..]),
                deps = deps,
                install_target = install_target
            ),
            true,
        )
    }

    fn installer_config(&mut self, path: &str) -> Result<()> {
        let data = read(path).with_context(|| format!("reading {}", path))?;
        // we don't validate but at least we parse
        serde_yaml::from_slice::<InstallConfig>(&data)
            .with_context(|| format!("parsing installer config {}", path))?;
        self.installer_config_bytes(&filename(path)?, &data)
    }

    fn installer_config_bytes(&mut self, filename: &str, data: &[u8]) -> Result<()> {
        if !self.features.installer_config {
            bail!("This OS image does not support customizing installer configuration.");
        }
        self.live.add_file(
            format!(
                "/etc/coreos/installer.d/{:04}-{}",
                self.installer_serial, filename
            ),
            data,
            0o600,
        )?;
        self.installer_serial += 1;
        Ok(())
    }

    fn live_config(&mut self, path: &str) -> Result<()> {
        let data = read(path).with_context(|| format!("reading {}", path))?;
        // we don't validate but at least we parse
        let (config, warnings) = ignition_config::Config::parse_slice(&data)
            .with_context(|| format!("parsing Ignition config {}", path))?;
        for warning in warnings {
            eprintln!("Warning parsing {}: {}", path, warning);
        }
        self.live
            .merge_config(&config)
            .with_context(|| format!("merging Ignition config {}", path))
    }

    fn into_initrd(mut self) -> Result<Initrd> {
        if self.dest.is_some() || !self.user_dest.is_empty() {
            // Embed dest config in live and installer configs

            // We now know we'll have a dest config, so add CAs to it
            for ca in self.dest_ca.drain(..) {
                self.dest.get_or_insert_with(Default::default).add_ca(&ca)?;
            }

            let data = if self.dest.is_none() && self.user_dest.len() == 1 {
                // Special case: the user supplied exactly one dest config
                // and we didn't add any dest config directives of our own.
                // Avoid another level of wrapping by embedding the user's
                // dest config directly.
                let mut buf = serde_json::to_vec(&self.user_dest.pop().unwrap())
                    .context("serializing dest Ignition config")?;
                buf.push(b'\n');
                buf
            } else {
                let dest = self.dest.get_or_insert_with(Default::default);
                for user_dest in self.user_dest.drain(..) {
                    dest.merge_config(&user_dest)?;
                }
                dest.to_bytes()?
            };
            let conf = self.installer.get_or_insert_with(Default::default);
            assert!(conf.ignition_file.is_none());
            let dest_path = "/etc/coreos/dest.ign";
            self.live.add_file(dest_path.into(), &data, 0o600)?;
            conf.ignition_file = Some(dest_path.into());
        }

        if self.installer_copy_network && (self.installer_serial > 0 || self.installer.is_some()) {
            // The installer will run, so have it copy network settings
            // to the destination
            self.installer
                .get_or_insert_with(Default::default)
                .copy_network = true;
        }

        if let Some(conf) = self.installer.take() {
            // Embed installer config in live config
            self.installer_config_bytes(
                "customize.yaml",
                &serde_yaml::to_vec(&conf).context("serializing installer config")?,
            )?;
        }

        // Embed live config in initrd
        self.initrd.add(INITRD_IGNITION_PATH, self.live.to_bytes()?);
        Ok(self.initrd)
    }
}

#[derive(Serialize)]
struct IsoInspectOutput {
    header: IsoFs,
    records: Vec<String>,
}

pub fn iso_inspect(config: IsoInspectConfig) -> Result<()> {
    let mut iso = IsoFs::from_file(open_live_iso(&config.input, None)?)?;
    let records = iso
        .walk()?
        .map(|r| r.map(|(path, _)| path))
        .collect::<Result<Vec<String>>>()
        .context("while walking ISO filesystem")?;
    let inspect_out = IsoInspectOutput {
        header: iso,
        records,
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer_pretty(&mut out, &inspect_out)
        .context("failed to serialize ISO metadata")?;
    out.write_all(b"\n").context("failed to write newline")?;
    Ok(())
}

pub fn iso_extract_pxe(config: IsoExtractPxeConfig) -> Result<()> {
    let mut iso = IsoFs::from_file(open_live_iso(&config.input, None)?)?;
    let pxeboot = iso.get_path(COREOS_ISO_PXEBOOT_DIR)?.try_into_dir()?;
    create_dir_all(&config.output_dir)?;

    let base = {
        // this can't be None since we successfully opened the live ISO at the location
        let mut s = Path::new(&config.input).file_stem().unwrap().to_os_string();
        s.push("-");
        s
    };

    for record in iso.list_dir(&pxeboot)? {
        match record? {
            iso9660::DirectoryRecord::Directory(_) => continue,
            iso9660::DirectoryRecord::File(file) => {
                let filename = {
                    let mut s = base.clone();
                    s.push(file.name.to_lowercase());
                    s
                };
                let path = Path::new(&config.output_dir).join(&filename);
                println!("{}", path.display());
                copy_file_from_iso(&mut iso, &file, &path)?;
            }
        }
    }
    Ok(())
}

fn copy_file_from_iso(iso: &mut IsoFs, file: &iso9660::File, output_path: &Path) -> Result<()> {
    let mut outf = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .with_context(|| format!("opening {}", output_path.display()))?;
    let mut bufw = BufWriter::with_capacity(BUFFER_SIZE, &mut outf);
    copy(&mut iso.read_file(file)?, &mut bufw)?;
    bufw.flush().context("flushing buffer")?;
    Ok(())
}

pub fn iso_extract_minimal_iso(config: IsoExtractMinimalIsoConfig) -> Result<()> {
    // Note we don't support overwriting the input ISO. Unlike other commands, this operation is
    // non-reversible, so let's make it harder for users to shoot themselves in the foot.
    let mut full_iso = IsoFs::from_file(open_live_iso(&config.input, None)?)?;

    // For now, we require the full ISO to be completely vanilla. Otherwise, the hashes won't
    // match.
    let iso = IsoConfig::for_iso(&mut full_iso)?;
    if iso.have_ignition() {
        bail!("Cannot operate on ISO with embedded Ignition config. Reset it and try again.");
    } else if iso.kargs()? != iso.kargs_default()? {
        bail!("Cannot operate on ISO with non-default kargs. Reset it and try again.");
    }

    // do this early so we exit immediately if stdout is a TTY
    let output_dir: PathBuf = if &config.output == "-" {
        verify_stdout_not_tty()?;
        std::env::temp_dir()
    } else {
        Path::new(&config.output)
            .parent()
            .with_context(|| format!("no parent directory of {}", &config.output))?
            .into()
    };

    if let Some(path) = &config.output_rootfs {
        let rootfs = full_iso
            .get_path(COREOS_ISO_ROOTFS_IMG)
            .with_context(|| format!("looking up '{}'", COREOS_ISO_ROOTFS_IMG))?
            .try_into_file()?;
        copy_file_from_iso(&mut full_iso, &rootfs, Path::new(path))?;
    }

    let miniso_data_file = full_iso
        .get_path(COREOS_ISO_MINISO_FILE)
        .with_context(|| format!("looking up '{}'", COREOS_ISO_MINISO_FILE))?
        .try_into_file()?;

    let data = {
        let mut f = full_iso.read_file(&miniso_data_file)?;
        miniso::Data::deserialize(&mut f).context("reading miniso data file")?
    };
    let mut outf = tempfile::Builder::new()
        .prefix(".coreos-installer-temp-")
        .tempfile_in(&output_dir)
        .context("creating temporary file")?;
    data.unxzpack(full_iso.as_file()?, &mut outf)
        .context("unpacking miniso")?;
    outf.seek(SeekFrom::Start(0))
        .context("seeking back to start of miniso tempfile")?;

    modify_miniso_kargs(outf.as_file_mut(), config.rootfs_url.as_ref())
        .context("modifying miniso kernel args")?;

    if &config.output == "-" {
        copy(&mut outf, &mut io::stdout().lock()).context("writing output")?;
    } else {
        outf.persist_noclobber(&config.output)
            .map_err(|e| e.error)?;
    }

    Ok(())
}

pub fn iso_pack_minimal_iso(config: IsoExtractPackMinimalIsoConfig) -> Result<()> {
    let mut full_iso = IsoFs::from_file(open_live_iso(&config.full, Some(None))?)?;
    let mut minimal_iso = IsoFs::from_file(open_live_iso(&config.minimal, None)?)?;

    let full_files = collect_iso_files(&mut full_iso)
        .with_context(|| format!("collecting files from {}", &config.full))?;
    let minimal_files = collect_iso_files(&mut minimal_iso)
        .with_context(|| format!("collecting files from {}", &config.minimal))?;
    if full_files.is_empty() {
        bail!("No files found in {}", &config.full);
    } else if minimal_files.is_empty() {
        bail!("No files found in {}", &config.minimal);
    }

    eprintln!("Packing minimal ISO");
    let (data, matches, skipped, written, written_compressed) =
        miniso::Data::xzpack(minimal_iso.as_file()?, &full_files, &minimal_files)
            .context("packing miniso")?;
    eprintln!("Matched {} files of {}", matches, minimal_files.len());

    eprintln!("Total bytes skipped: {}", skipped);
    eprintln!("Total bytes written: {}", written);
    eprintln!("Total bytes written (compressed): {}", written_compressed);

    eprintln!("Verifying that packed image matches digest");
    data.unxzpack(full_iso.as_file()?, std::io::sink())
        .context("unpacking miniso for verification")?;

    let miniso_entry = full_iso
        .get_path(COREOS_ISO_MINISO_FILE)
        .with_context(|| format!("looking up '{}'", COREOS_ISO_MINISO_FILE))?
        .try_into_file()?;
    let mut w = full_iso.overwrite_file(&miniso_entry)?;
    data.serialize(&mut w).context("writing miniso data file")?;
    w.flush().context("flushing full ISO")?;

    if config.consume {
        std::fs::remove_file(&config.minimal)
            .with_context(|| format!("consuming {}", &config.minimal))?;
    }

    eprintln!("Packing successful!");
    Ok(())
}

fn collect_iso_files(iso: &mut IsoFs) -> Result<HashMap<String, iso9660::File>> {
    iso.walk()?
        .filter_map(|r| match r {
            Err(e) => Some(Err(e)),
            Ok((s, iso9660::DirectoryRecord::File(f))) => Some(Ok((s, f))),
            Ok(_) => None,
        })
        .collect::<Result<HashMap<String, iso9660::File>>>()
        .context("while walking ISO filesystem")
}

fn modify_miniso_kargs(f: &mut File, rootfs_url: Option<&String>) -> Result<()> {
    let mut iso = IsoFs::from_file(f.try_clone().context("cloning a file")?)?;
    let mut cfg = IsoConfig::for_file(f)?;

    let kargs = cfg.kargs()?;

    // same disclaimer as `modify_kargs()` here re. whitespace/quoting
    let liveiso_karg = kargs
        .split_ascii_whitespace()
        .find(|&karg| karg.starts_with("coreos.liveiso="))
        .context("minimal ISO does not have coreos.liveiso= karg")?
        .to_string();

    let new_default_kargs = KargsEditor::new().delete(&[liveiso_karg]).apply_to(kargs)?;
    cfg.set_kargs(&new_default_kargs)?;

    if let Some(url) = rootfs_url {
        if url.split_ascii_whitespace().count() > 1 {
            bail!("forbidden whitespace found in '{}'", url);
        }
        let final_kargs = KargsEditor::new()
            .append(&[format!("coreos.live.rootfs_url={}", url)])
            .apply_to(&new_default_kargs)?;

        cfg.set_kargs(&final_kargs)?;
    }

    // update kargs
    write_live_iso(&cfg, f, None)?;

    // also modify the default kargs because we don't want `coreos-installer iso kargs reset` to
    // re-add `coreos.liveiso`
    let mut kargs_info = KargEmbedInfo::for_iso(&mut iso)?.context(
        // should be impossible; we only support new-style CoreOS ISOs with kargs.json
        "minimal ISO does not have kargs.json; please report this as a bug",
    )?;

    // NB: We don't need to update the length for this; it's a fixed property of the kargs files.
    // (Though its original value did depend on the original default kargs at build time.)
    kargs_info.default = new_default_kargs;
    kargs_info.update_iso(&mut iso)?;

    Ok(())
}

fn verify_stdout_not_tty() -> Result<()> {
    if isatty(io::stdout().as_raw_fd()).context("checking if stdout is a TTY")? {
        bail!("Refusing to write binary data to terminal");
    }
    Ok(())
}

fn filename(path: &str) -> Result<String> {
    Ok(Path::new(path)
        .file_name()
        .with_context(|| format!("missing filename in {}", path))?
        // path was originally a string
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::copy;

    use tempfile::tempfile;
    use xz2::read::XzDecoder;

    fn open_iso_file() -> File {
        let iso_bytes: &[u8] = include_bytes!("../fixtures/iso/embed-areas-2021-09.iso.xz");
        let mut decoder = XzDecoder::new(iso_bytes);
        let mut iso_file = tempfile().unwrap();
        copy(&mut decoder, &mut iso_file).unwrap();
        iso_file
    }

    #[test]
    fn test_initrd_embed_area() {
        let mut iso_file = open_iso_file();
        // normal read
        let mut iso = IsoFs::from_file(iso_file.try_clone().unwrap()).unwrap();
        let area = InitrdEmbedArea::for_iso(&mut iso).unwrap();
        assert_eq!(area.region.offset, 102400);
        assert_eq!(area.region.length, 262144);
        // missing embed area
        iso_file.seek(SeekFrom::Start(65903)).unwrap();
        iso_file.write_all(b"Z").unwrap();
        let mut iso = IsoFs::from_file(iso_file).unwrap();
        InitrdEmbedArea::for_iso(&mut iso).unwrap_err();
    }

    #[test]
    fn test_karg_embed_area() {
        let mut iso_file = open_iso_file();
        // normal read
        check_karg_embed_areas(&mut iso_file);
        // JSON only
        iso_file.seek(SeekFrom::Start(32672)).unwrap();
        iso_file.write_all(&[0; 8]).unwrap();
        check_karg_embed_areas(&mut iso_file);
        // legacy header only
        iso_file.seek(SeekFrom::Start(32672)).unwrap();
        iso_file.write_all(b"coreKarg").unwrap();
        iso_file.seek(SeekFrom::Start(63725)).unwrap();
        iso_file.write_all(b"Z").unwrap();
        check_karg_embed_areas(&mut iso_file);
        // neither header
        iso_file.seek(SeekFrom::Start(32672)).unwrap();
        iso_file.write_all(&[0; 8]).unwrap();
        let mut iso = IsoFs::from_file(iso_file).unwrap();
        assert!(KargEmbedAreas::for_iso(&mut iso).unwrap().is_none());
    }

    fn check_karg_embed_areas(iso_file: &mut File) {
        let iso_file = iso_file.try_clone().unwrap();
        let mut iso = IsoFs::from_file(iso_file).unwrap();
        let areas = KargEmbedAreas::for_iso(&mut iso).unwrap().unwrap();
        assert_eq!(areas.length, 1139);
        assert_eq!(areas.default, "mitigations=auto,nosmt coreos.liveiso=fedora-coreos-34.20210921.dev.0 ignition.firstboot ignition.platform.id=metal");
        assert_eq!(areas.regions.len(), 2);
        assert_eq!(areas.regions[0].offset, 98126);
        assert_eq!(areas.regions[0].length, 1139);
        assert_eq!(areas.regions[1].offset, 371658);
        assert_eq!(areas.regions[1].length, 1139);
    }
}
