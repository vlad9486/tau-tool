use std::{fs, io, path::Path, process::Output};

use object::{
    Object,
    read::elf::{ElfFile64, ProgramHeader},
};
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
    #[error("expected a little-endian RISC-V 64 ELF")]
    ElfFormat,
    #[error("ELF has no file-backed loadable segments")]
    ElfEmpty,
    #[error("loadable segments overlap")]
    ElfOverlap,
    #[error("raw image exceeds its size limit")]
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

/// Flatten file-backed PT_LOAD segments at their physical/load addresses.
/// Gaps are zero-filled; leading/trailing BSS is not emitted. `max_size` bounds
/// the full address span before allocating. The caller/firmware initializes BSS.
pub fn elf_to_raw(data: &[u8], max_size: usize) -> Result<Vec<u8>, ElfError> {
    let file = ElfFile64::<object::Endianness>::parse(data)?;
    if file.architecture() != object::Architecture::Riscv64 || !file.is_little_endian() {
        return Err(ElfError::ElfFormat);
    }
    let endian = file.endian();
    let mut loads = Vec::new();
    for segment in file.elf_program_headers() {
        if segment.p_type(endian) != object::elf::PT_LOAD {
            continue;
        }
        let address = segment.p_paddr(endian);
        let filesz = segment.p_filesz(endian);
        let memsz = segment.p_memsz(endian);
        if filesz > memsz || address.checked_add(memsz).is_none() {
            return Err(ElfError::ElfSegment);
        }
        if filesz == 0 {
            continue;
        }
        let bytes = segment
            .data(endian, data)
            .map_err(|_| ElfError::ElfSegment)?;
        loads.push((address, bytes));
    }
    loads.sort_unstable_by_key(|(address, _)| *address);
    let base = loads.first().ok_or(ElfError::ElfEmpty)?.0;
    let mut end = base;
    for (address, bytes) in &loads {
        if *address < end {
            return Err(ElfError::ElfOverlap);
        }
        end = address
            .checked_add(bytes.len() as u64)
            .ok_or(ElfError::ElfSegment)?;
    }
    let size = usize::try_from(end - base).map_err(|_| ElfError::ElfOutputTooSmall)?;
    if size > max_size {
        return Err(ElfError::ElfOutputTooSmall);
    }
    let mut image = vec![0; size];
    for (address, bytes) in loads {
        let offset = (address - base) as usize;
        image[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
    Ok(image)
}

/// Compose the normal loader/supervisor with the selected user payload ELF.
pub fn compose_tau_image_from(directory: &Path, payload: &str) -> Result<Vec<u8>, ComposeError> {
    const SUPERVISOR_OFFSET: usize = 0x5000;
    const SYSTEM_OFFSET: usize = 0x10000;

    let mut image = vec![0; 0x200000];
    let path = directory.join("loader");
    let data = fs::read(&path).map_err(|err| ComposeError::io(&path.to_string_lossy(), err))?;
    let raw = elf_to_raw(&data, SUPERVISOR_OFFSET)
        .map_err(|err| ComposeError::err(&path.to_string_lossy(), err))?;
    image[..raw.len()].copy_from_slice(&raw);
    let path = directory.join("supervisor");
    let data = fs::read(&path).map_err(|err| ComposeError::io(&path.to_string_lossy(), err))?;
    let raw = elf_to_raw(&data, SYSTEM_OFFSET - SUPERVISOR_OFFSET)
        .map_err(|err| ComposeError::err(&path.to_string_lossy(), err))?;
    image[SUPERVISOR_OFFSET..SUPERVISOR_OFFSET + raw.len()].copy_from_slice(&raw);
    let path = directory.join(payload);
    let mut file =
        fs::File::open(&path).map_err(|err| ComposeError::io(&path.to_string_lossy(), err))?;
    let mut w = &mut image[SYSTEM_OFFSET..];
    let copied = io::copy(&mut file, &mut w)
        .map_err(|err| ComposeError::io(&path.to_string_lossy(), err))?;
    image.truncate(SYSTEM_OFFSET + copied as usize);

    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal RV64 ELF, deliberately without section headers: conversion uses
    // program headers only. Every virtual address differs from its load address.
    fn fixture(segments: &[(u64, &[u8], u64)]) -> Vec<u8> {
        let mut elf = vec![0u8; 64 + segments.len() * 56];
        elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        elf[16..18].copy_from_slice(&2u16.to_le_bytes());
        elf[18..20].copy_from_slice(&243u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1u32.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[52..54].copy_from_slice(&64u16.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        for (index, &(address, bytes, memsz)) in segments.iter().enumerate() {
            let header = 64 + index * 56;
            elf[header..header + 4].copy_from_slice(&1u32.to_le_bytes());
            for (offset, value) in [
                (8, elf.len() as u64),
                (16, 0xffff_ffc0_0000_0000 + (index as u64 * 0x1000)),
                (24, address),
                (32, bytes.len() as u64),
                (40, memsz),
                (48, 1),
            ] {
                elf[header + offset..header + offset + 8].copy_from_slice(&value.to_le_bytes());
            }
            elf.extend_from_slice(bytes);
        }
        elf
    }

    #[test]
    fn raw_uses_load_addresses_and_zero_fills_gaps_without_trailing_bss() {
        let elf = fixture(&[
            (0x8008, &[3, 4], 0x1000), // Out of order, with trailing BSS.
            (0x7000, &[], 0x100),      // Leading BSS does not move the image base.
            (0x8000, &[1, 2], 8),
        ]);
        assert_eq!(
            elf_to_raw(&elf, 10).unwrap(),
            [1, 2, 0, 0, 0, 0, 0, 0, 3, 4]
        );
        assert!(matches!(
            elf_to_raw(&elf, 9),
            Err(ElfError::ElfOutputTooSmall)
        ));
    }

    #[test]
    fn raw_rejects_truncated_invalid_and_overlapping_segments() {
        let mut elf = fixture(&[(0x8000, &[1, 2], 2)]);
        elf.pop();
        assert!(matches!(elf_to_raw(&elf, 100), Err(ElfError::ElfSegment)));
        for segments in [
            vec![(0x8000, &[1, 2][..], 1)],
            vec![(u64::MAX, &[1][..], 1)],
        ] {
            assert!(matches!(
                elf_to_raw(&fixture(&segments), 100),
                Err(ElfError::ElfSegment)
            ));
        }
        let elf = fixture(&[(0x8000, &[1, 2], 2), (0x8001, &[3], 1)]);
        assert!(matches!(elf_to_raw(&elf, 100), Err(ElfError::ElfOverlap)));
    }

    #[test]
    fn raw_rejects_empty_images_and_excessive_address_spans() {
        let elf = fixture(&[(0x8000, &[], 0x1000)]);
        assert!(matches!(elf_to_raw(&elf, 100), Err(ElfError::ElfEmpty)));
        let elf = fixture(&[(0, &[1], 1), (1 << 40, &[2], 1)]);
        assert!(matches!(
            elf_to_raw(&elf, 100),
            Err(ElfError::ElfOutputTooSmall)
        ));
        assert!(elf_to_raw(b"not an ELF", 100).is_err());
    }
}
