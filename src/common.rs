use std::{
    cmp, fs, io,
    process::{Command, Output, Stdio},
};

use object::{Object, ObjectSegment};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{file} {err}")]
pub struct ComposeError {
    file: String,
    err: ElfError,
}

impl ComposeError {
    pub fn io(path: &str, err: io::Error) -> Self {
        Self::err(path, ElfError::Read(err))
    }

    pub fn err(path: &str, err: ElfError) -> Self {
        ComposeError {
            file: path.to_owned(),
            err,
        }
    }
}

#[derive(Debug, Error)]
pub enum ElfError {
    #[error("read error: {0}")]
    Read(#[from] io::Error),
    #[error("elf: {0}")]
    ElfParse(#[from] object::read::Error),
    #[error("segment range invalid or truncated")]
    ElfSegment,
    #[error("output image is too small")]
    ElfOutputTooSmall,
}

#[derive(Debug, Error)]
#[error("build error")]
pub enum BuildError {
    #[error("invoke cargo error: {0}")]
    Invocation(#[from] io::Error),
    #[error("cargo error")]
    Cargo,
}

pub fn bail<E>(out: &Output, msg: impl Fn() -> E) -> Result<(), E> {
    if !out.status.success() {
        Err(msg())
    } else {
        Ok(())
    }
}

fn elf_to_raw(data: &[u8], image: &mut [u8]) -> Result<(), ElfError> {
    let file = object::File::parse(data)?;

    let mut min_addr = u64::MAX;
    let mut max_addr = 0u64;
    let mut loads = Vec::new();

    for seg in file.segments() {
        // filter by seg.kind() to keep only PT_LOAD segments
        let vaddr = seg.address(); // ELF p_vaddr
        let filesz = seg.data().map(<[u8]>::len).unwrap_or_default() as u64; // p_filesz
        let memsz = seg.size(); // p_memsz

        if seg.size() == 0 {
            continue;
        }
        // Track overall extent using p_memsz, but we copy only p_filesz bytes.
        min_addr = cmp::min(min_addr, vaddr);
        max_addr = cmp::max(max_addr, vaddr.saturating_add(memsz));
        loads.push((vaddr, filesz, seg));
    }

    for (vaddr, filesz, seg) in loads {
        if filesz == 0 {
            continue; // pure BSS portion in this segment—already zeroed
        }
        let bytes = seg.data().unwrap_or(&[]);
        let off = (vaddr - min_addr) as usize;
        let end = off + (filesz as usize);
        if end > image.len() || bytes.len() < filesz as usize {
            return Err(ElfError::ElfSegment);
        }
        if image.len() < end {
            return Err(ElfError::ElfOutputTooSmall);
        }
        image[off..end].copy_from_slice(&bytes[..filesz as usize]);
    }

    Ok(())
}

pub fn build_tau() -> Result<(), BuildError> {
    let out = Command::new("cargo")
        .env("RUSTFLAGS", "-C relocation-model=pie")
        .args([
            "build",
            "--release",
            "--package=supervisor",
            "--features=panic-never",
            "--bin=loader",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    bail(&out, || BuildError::Cargo)?;

    let out = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--package=supervisor",
            "--features=panic-never",
            "--bin=supervisor",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    bail(&out, || BuildError::Cargo)?;

    let out = Command::new("cargo")
        .args(["build", "--release", "--package=system", "--bin=system"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    bail(&out, || BuildError::Cargo)?;

    Ok(())
}

pub fn compose_tau_image() -> Result<Vec<u8>, ComposeError> {
    const SUPERVISOR_OFFSET: usize = 0x5000;
    const SYSTEM_OFFSET: usize = 0x10000;

    let mut image = vec![0; 0x200000];
    let path = "target/riscv64imac-unknown-none-elf/release/loader";
    let data = fs::read(path).map_err(|err| ComposeError::io(path, err))?;
    elf_to_raw(&data, &mut image[..SUPERVISOR_OFFSET])
        .map_err(|err| ComposeError::err(path, err))?;
    let path = "target/riscv64imac-unknown-none-elf/release/supervisor";
    let data = fs::read(path).map_err(|err| ComposeError::io(path, err))?;
    elf_to_raw(&data, &mut image[SUPERVISOR_OFFSET..SYSTEM_OFFSET])
        .map_err(|err| ComposeError::err(path, err))?;
    let path = "target/riscv64imac-unknown-none-elf/release/system";
    let mut file = fs::File::open(path).map_err(|err| ComposeError::io(path, err))?;
    let mut w = &mut image[SYSTEM_OFFSET..];
    let copied = io::copy(&mut file, &mut w).map_err(|err| ComposeError::io(path, err))?;
    image.truncate(SYSTEM_OFFSET + copied as usize);

    Ok(image)
}
