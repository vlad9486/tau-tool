use anyhow::{Context, Result, bail, ensure};

pub const SUPERVISOR_TIMER: u64 = 5;
pub const SUPERVISOR_EXTERNAL: u64 = 9;

#[derive(Clone, Debug)]
pub struct TrapSnapshot {
    pub scause: u64,
    pub sepc: u64,
    pub stval: u64,
    pub sstatus: u64,
    pub satp: u64,
}

impl TrapSnapshot {
    pub fn is_interrupt(&self) -> bool {
        self.scause >> 63 != 0
    }
    pub fn cause(&self) -> u64 {
        self.scause & !(1u64 << 63)
    }
}

/// Resolve stvec's direct or vectored entry. None means a synchronous exception.
pub fn trap_address(stvec: u64, interrupt: Option<u64>) -> Result<u64> {
    let base = stvec & !3;
    match stvec & 3 {
        0 => Ok(base),
        1 => base
            .checked_add(
                interrupt
                    .unwrap_or(0)
                    .checked_mul(4)
                    .context("trap vector overflow")?,
            )
            .context("trap vector overflow"),
        mode => bail!("unsupported stvec mode {mode}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagingMode {
    Bare,
    Sv39,
    Sv48,
    Sv57,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PagePermissions {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
    pub user: bool,
    pub global: bool,
    pub accessed: bool,
    pub dirty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Translation {
    pub mode: PagingMode,
    pub physical_address: u64,
    /// Zero for Bare mode; otherwise the leaf's mapping size, including superpages.
    pub page_size: u64,
    pub permissions: Option<PagePermissions>,
    pub pte_address: Option<u64>,
    pub pte: Option<u64>,
}

/// Walk a base RV64 page table using physical PTE reads.
/// Does not evaluate PMP, SUM, MXR, current privilege, or perform A/D updates.
/// Extension-specific PTE encodings (including Svnapot/Svpbmt) are rejected.
pub fn translate(
    satp: u64,
    address: u64,
    mut read_pte: impl FnMut(u64) -> Result<u64>,
) -> Result<Translation> {
    let (mode, levels) = match satp >> 60 {
        0 => {
            return Ok(Translation {
                mode: PagingMode::Bare,
                physical_address: address,
                page_size: 0,
                permissions: None,
                pte_address: None,
                pte: None,
            });
        }
        8 => (PagingMode::Sv39, 3),
        9 => (PagingMode::Sv48, 4),
        10 => (PagingMode::Sv57, 5),
        value => bail!("unsupported satp mode {value}"),
    };
    let bits = 12 + levels * 9;
    let canonical = (((address << (64 - bits)) as i64) >> (64 - bits)) as u64;
    ensure!(
        address == canonical,
        "noncanonical {mode:?} virtual address {address:#x}"
    );
    let ppn_mask = (1u64 << 44) - 1;
    let mut table = (satp & ppn_mask) << 12;
    let mut inherited_global = false;
    for level in (0..levels).rev() {
        let index = (address >> (12 + level * 9)) & 0x1ff;
        let pte_address = table + index * 8;
        let pte = read_pte(pte_address)
            .with_context(|| format!("read level {level} PTE at {pte_address:#x}"))?;
        ensure!(
            pte >> 54 == 0,
            "reserved or extension-specific PTE bits at {pte_address:#x}: {pte:#x}"
        );
        let read = pte & 2 != 0;
        let write = pte & 4 != 0;
        let execute = pte & 8 != 0;
        ensure!(
            pte & 1 != 0 && (!write || read),
            "invalid PTE at {pte_address:#x}: {pte:#x}"
        );
        inherited_global |= pte & 32 != 0;
        let ppn = (pte >> 10) & ppn_mask;
        if read || execute {
            let page_size = 1u64 << (12 + level * 9);
            let physical_base = ppn << 12;
            ensure!(
                physical_base & (page_size - 1) == 0,
                "misaligned superpage at {pte_address:#x}"
            );
            return Ok(Translation {
                mode,
                physical_address: physical_base | (address & (page_size - 1)),
                page_size,
                permissions: Some(PagePermissions {
                    read,
                    write,
                    execute,
                    user: pte & 16 != 0,
                    global: inherited_global,
                    accessed: pte & 64 != 0,
                    dirty: pte & 128 != 0,
                }),
                pte_address: Some(pte_address),
                pte: Some(pte),
            });
        }
        ensure!(
            pte & (16 | 64 | 128) == 0,
            "reserved U/A/D bits on non-leaf PTE at {pte_address:#x}"
        );
        table = ppn << 12;
    }
    bail!("page table has no leaf for {address:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn direct_and_vectored_traps() {
        assert_eq!(
            trap_address(0xffff_ffc0_0000_1000, Some(9)).unwrap(),
            0xffff_ffc0_0000_1000
        );
        assert_eq!(trap_address(0x1001, Some(9)).unwrap(), 0x1024);
        assert_eq!(trap_address(0x1001, None).unwrap(), 0x1000);
        assert!(trap_address(0x1002, None).is_err());
    }

    #[test]
    fn walks_high_virtual_address_and_inherits_global() {
        let va = 0xffff_ffc0_0123_4567u64;
        let mut memory = HashMap::new();
        memory.insert(0x1000 + ((va >> 30) & 511) * 8, (2 << 10) | 1 | 32);
        memory.insert(0x2000 + ((va >> 21) & 511) * 8, (3 << 10) | 1);
        memory.insert(
            0x3000 + ((va >> 12) & 511) * 8,
            (0x80205 << 10) | 1 | 2 | 8 | 64,
        );
        let t = translate((8 << 60) | 1, va, |a| Ok(memory[&a])).unwrap();
        assert_eq!(t.physical_address, 0x8020_5567);
        assert_eq!(t.page_size, 4096);
        let p = t.permissions.unwrap();
        assert!(p.execute && p.read && p.global && !p.write && !p.user);
    }

    #[test]
    fn superpages_and_invalid_entries() {
        let satp = (8 << 60) | 1;
        let leaf = ((0x8000_0000u64 >> 12) << 10) | 0xcf;
        let t = translate(satp, 0x1234, |_| Ok(leaf)).unwrap();
        assert_eq!(t.page_size, 1 << 30);
        assert_eq!(t.physical_address, 0x8000_1234);
        assert!(translate(satp, 0, |_| Ok(leaf | (1 << 10))).is_err());
        assert!(translate(satp, 0, |_| Ok(5)).is_err());
        assert!(translate(satp, 0, |_| Ok(0)).is_err());
        assert!(translate(satp, 0, |_| Ok(leaf | (1 << 63))).is_err());
        assert!(translate(satp, 1 << 39, |_| panic!("must reject before reading")).is_err());
    }

    #[test]
    fn other_modes_and_bare() {
        assert_eq!(
            translate(0, u64::MAX, |_| panic!())
                .unwrap()
                .physical_address,
            u64::MAX
        );
        for (mode, levels) in [(9u64, 4), (10, 5)] {
            let mut calls = 0;
            let t = translate((mode << 60) | 1, 0x123, |_| {
                calls += 1;
                Ok(if calls == levels {
                    (0x12345 << 10) | 0xc7
                } else {
                    (2 << 10) | 1
                })
            })
            .unwrap();
            assert_eq!(calls, levels);
            assert_eq!(t.physical_address, 0x1234_5123);
        }
    }
}
