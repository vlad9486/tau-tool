use std::{fs, io, path::Path, process::Output};

use object::{
    Object, ObjectSection,
    build::elf::{Builder, SectionData},
    read::elf::{ElfFile64, ProgramHeader},
};
use thiserror::Error;

use super::modules;

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
    #[error("module layout: {0}")]
    Layout(String),
    #[error("strip debug information: {0}")]
    Strip(String),
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
    #[error("module layout: {0}")]
    Layout(String),
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

/// Strip non-runtime symbols and sections in memory using `object`.
/// Never modify the original ELF: the debugger loads its symbols and source info.
fn stripped_elf(path: &Path) -> Result<Vec<u8>, ElfError> {
    let data = fs::read(path)?;
    let file = ElfFile64::<object::Endianness>::parse(data.as_slice())?;
    if file.architecture() != object::Architecture::Riscv64 || !file.is_little_endian() {
        return Err(ElfError::ElfFormat);
    }
    // Already stripped ELFs can have segment data without any section headers.
    // The builder writes section data only, so leave these files intact.
    if file.elf_section_table().is_empty() {
        return Ok(data);
    }
    let mut builder =
        Builder::read(data.as_slice()).map_err(|error| ElfError::Strip(error.to_string()))?;
    for section in &mut builder.sections {
        // Keep runtime data, section names, and architecture attributes.
        section.delete = !section.is_alloc()
            && !matches!(
                section.data,
                SectionData::SectionString | SectionData::Attributes(_)
            );
    }
    for symbol in &mut builder.symbols {
        symbol.delete = true;
    }
    let mut packed = Vec::new();
    builder
        .write(&mut packed)
        .map_err(|error| ElfError::Strip(error.to_string()))?;

    // The builder preserves program headers, but relocates non-allocated
    // sections. Update the informational RISC-V attributes segment accordingly.
    let file = ElfFile64::<object::Endianness>::parse(packed.as_slice())?;
    let endian = file.endian();
    let attributes = file
        .section_by_name(".riscv.attributes")
        .and_then(|section| section.file_range());
    let header_count = file.elf_program_headers().len();
    // Builder writes the program header table immediately after the ELF header.
    let header_size = size_of::<object::elf::FileHeader64<object::Endianness>>();
    let (headers, _) = object::pod::slice_from_bytes_mut::<
        object::elf::ProgramHeader64<object::Endianness>,
    >(&mut packed[header_size..], header_count)
    .map_err(|_| ElfError::ElfSegment)?;
    for header in headers {
        if header.p_type(endian) != object::elf::PT_RISCV_ATTRIBUTES {
            continue;
        }
        let (offset, size) = attributes
            .ok_or_else(|| ElfError::Strip("missing RISC-V attributes section".into()))?;
        header.p_offset.set(endian, offset);
        header.p_filesz.set(endian, size);
        header.p_memsz.set(endian, size);
    }
    Ok(packed)
}

/// Compose all static modules. S-mode binaries are flat images; U-mode modules
/// remain ELF, with debug information removed only from the packed copy.
pub fn compose_tau_image_from(directory: &Path, manifest: &Path) -> Result<Vec<u8>, ComposeError> {
    let config = modules::Modules::read(manifest).map_err(|error| {
        ComposeError::err(
            &manifest.to_string_lossy(),
            ElfError::Layout(format!("{error:#}")),
        )
    })?;
    let end = config
        .module
        .last()
        .map_or(0, |m| (m.offset + m.size) << 12);
    let mut image = vec![0; end];
    for module in config.module {
        let path = directory.join(&module.name);
        let result = (|| {
            let size = module.size << 12;
            let bytes = if matches!(module.name.as_str(), "loader" | "supervisor") {
                elf_to_raw(&fs::read(&path)?, size)?
            } else {
                stripped_elf(&path)?
            };
            if bytes.len() > size {
                return Err(ElfError::ElfOutputTooSmall);
            }
            let start = module.offset << 12;
            image[start..start + bytes.len()].copy_from_slice(&bytes);
            Ok(())
        })();
        result.map_err(|err| ComposeError::err(&path.to_string_lossy(), err))?;
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripping_preserves_load_segments_and_original_debug_file() {
        let mut data = fixture(&[(0x8020_0000, &[0x73, 0, 0, 0], 4)]);
        let names = b"\0.text\0.debug_info\0.shstrtab\0";
        let names_offset = data.len();
        data.extend_from_slice(names);
        let debug_offset = data.len();
        data.extend_from_slice(b"debug fixture");
        let table = data.len().next_multiple_of(8);
        data.resize(table + 4 * 64, 0);
        data[40..48].copy_from_slice(&(table as u64).to_le_bytes());
        data[58..60].copy_from_slice(&64u16.to_le_bytes());
        data[60..62].copy_from_slice(&4u16.to_le_bytes());
        data[62..64].copy_from_slice(&3u16.to_le_bytes());
        for (index, name, kind, flags, address, offset, size) in [
            (1, 1u32, 1u32, 6u64, 0x8020_0000u64, 120, 4),
            (2, 7, 1, 0, 0, debug_offset, 13),
            (3, 19, 3, 0, 0, names_offset, names.len()),
        ] {
            let section = table + index * 64;
            data[section..section + 4].copy_from_slice(&name.to_le_bytes());
            data[section + 4..section + 8].copy_from_slice(&kind.to_le_bytes());
            for (field, value) in [
                (8, flags),
                (16, address),
                (24, offset as u64),
                (32, size as u64),
                (48, 1),
            ] {
                data[section + field..section + field + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        // Add BSS, a static symbol table, and a non-allocated attributes segment.
        // Its original offset deliberately differs from the stripped location.
        let mut builder = Builder::read(data.as_slice()).unwrap();
        let text = builder
            .sections
            .iter_mut()
            .find(|s| s.name.as_slice() == b".text")
            .unwrap();
        text.sh_offset = 0x1000;
        let text_id = text.id();
        let load = builder.segments.iter_mut().next().unwrap();
        load.p_offset = 0x1000;
        load.p_vaddr = 0x8020_0000;
        load.p_memsz = 12;
        builder.header.e_entry = load.p_vaddr;
        let bss = builder.sections.add();
        bss.name = b".bss".as_slice().into();
        bss.sh_type = object::elf::SHT_NOBITS;
        bss.sh_flags = object::elf::SHF_ALLOC | object::elf::SHF_WRITE;
        bss.sh_addr = 0x8020_0004;
        bss.sh_offset = 0x1004;
        bss.sh_addralign = 4;
        bss.data = SectionData::UninitializedData(8);
        let symbol = builder.symbols.add();
        symbol.name = b"entry".as_slice().into();
        symbol.section = Some(text_id);
        symbol.st_value = 0x8020_0000;
        symbol.set_st_info(object::elf::STB_GLOBAL, object::elf::STT_FUNC);
        for (name, kind, contents) in [
            (
                b".symtab".as_slice(),
                object::elf::SHT_SYMTAB,
                SectionData::Symbol,
            ),
            (
                b".strtab".as_slice(),
                object::elf::SHT_STRTAB,
                SectionData::String,
            ),
        ] {
            let section = builder.sections.add();
            section.name = name.into();
            section.sh_type = kind;
            section.sh_addralign = 8;
            section.data = contents;
        }
        let attributes = builder.sections.add();
        attributes.name = b".riscv.attributes".as_slice().into();
        attributes.sh_type = object::elf::SHT_RISCV_ATTRIBUTES;
        attributes.sh_addralign = 1;
        attributes.data = SectionData::Attributes(object::build::elf::AttributesSection::new());
        let segment = builder.segments.add();
        segment.p_type = object::elf::PT_RISCV_ATTRIBUTES;
        segment.p_flags = object::elf::PF_R;
        segment.p_align = 1;
        let mut data = Vec::new();
        builder.write(&mut data).unwrap();
        let file = object::File::parse(data.as_slice()).unwrap();
        let (offset, size) = file
            .section_by_name(".riscv.attributes")
            .unwrap()
            .file_range()
            .unwrap();
        let (header, _) = object::pod::from_bytes_mut::<
            object::elf::ProgramHeader64<object::LittleEndian>,
        >(&mut data[120..])
        .unwrap();
        header.p_offset.set(object::LittleEndian, offset);
        header.p_filesz.set(object::LittleEndian, size);
        header.p_memsz.set(object::LittleEndian, size);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("system");
        fs::write(&path, &data).unwrap();
        let stripped = stripped_elf(&path).unwrap();
        let original = object::File::parse(data.as_slice()).unwrap();
        let packed = object::File::parse(stripped.as_slice()).unwrap();
        assert!(original.section_by_name(".debug_info").is_some());
        assert!(packed.section_by_name(".debug_info").is_none());
        assert_eq!(original.entry(), packed.entry());
        assert!(original.symbol_by_name("entry").is_some());
        assert!(packed.symbols().next().is_none());
        assert!(packed.section_by_name(".symtab").is_none());
        assert!(packed.section_by_name(".strtab").is_none());
        assert_eq!(packed.section_by_name(".bss").unwrap().size(), 8);
        let original_elf = ElfFile64::<object::Endianness>::parse(data.as_slice()).unwrap();
        let packed_elf = ElfFile64::<object::Endianness>::parse(stripped.as_slice()).unwrap();
        let endian = original_elf.endian();
        assert_eq!(
            object::pod::bytes_of(&original_elf.elf_program_headers()[0]),
            object::pod::bytes_of(&packed_elf.elf_program_headers()[0])
        );
        let attributes = packed.section_by_name(".riscv.attributes").unwrap();
        let header = &packed_elf.elf_program_headers()[1];
        assert_eq!(
            (header.p_offset(endian), header.p_filesz(endian)),
            attributes.file_range().unwrap()
        );
        assert_ne!(header.p_offset(endian), offset);
        assert_eq!(
            header.data(endian, stripped.as_slice()).unwrap(),
            attributes.data().unwrap()
        );
        assert_eq!(
            elf_to_raw(&data, 4096).unwrap(),
            elf_to_raw(&stripped, 4096).unwrap()
        );
        assert_eq!(fs::read(path).unwrap(), data);
        assert!(stripped.len() < data.len());
    }

    #[test]
    fn stripping_preserves_sectionless_elf_and_rejects_invalid_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("module");
        let data = fixture(&[(0x8000, &[1, 2, 3, 4], 8)]);
        fs::write(&path, &data).unwrap();
        assert_eq!(stripped_elf(&path).unwrap(), data);
        let mut wrong_architecture = data.clone();
        wrong_architecture[18..20].copy_from_slice(&62u16.to_le_bytes());
        fs::write(&path, wrong_architecture).unwrap();
        assert!(matches!(stripped_elf(&path), Err(ElfError::ElfFormat)));
        fs::write(&path, b"not an ELF").unwrap();
        assert!(matches!(stripped_elf(&path), Err(ElfError::ElfParse(_))));
        assert!(matches!(
            stripped_elf(&directory.path().join("missing")),
            Err(ElfError::Read(_))
        ));
    }

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
