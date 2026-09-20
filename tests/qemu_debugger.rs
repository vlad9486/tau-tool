#![cfg(unix)]

use std::{fs, time::Duration};
use anyhow::Result;
use tau_builder::debugger::{ElfImage, SymbolMapping, TestVm, VmConfig};

/// Exercises real QEMU and GDB without requiring the separate OS repository or a cross compiler.
#[test]
#[ignore = "requires qemu-system-riscv64 and gdb-multiarch"]
fn real_qemu_registers_memory_breakpoints_and_cleanup() -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("tau smoke with spaces ")
        .tempdir()?;
    // addi s0, zero, 42; jal zero, 0. Raw firmware loaded at 0x80000000.
    let firmware: Vec<u8> = [0x02a00413u32, 0x0000006f]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    fs::write(root.path().join("firmware.bin"), &firmware)?;
    let symbols = root.path().join("symbols with spaces.elf");
    fs::write(&symbols, symbol_elf(&firmware))?;
    let mut config = VmConfig::new(root.path());
    config.firmware = "firmware.bin".into();
    config.memory_mib = 128;
    let mut vm = TestVm::spawn(config)?;
    vm.check(|vm| {
        assert_eq!(vm.qmp().status()?["running"], false);
        let dbg = vm.debugger();
        assert_eq!(dbg.threads()?.len(), 4);
        let image = ElfImage::read(&symbols)?;
        assert_eq!(
            image.runtime_entry(SymbolMapping::LoadAt(0x8000_0000)),
            0x8000_0000
        );
        dbg.add_symbols(&image, SymbolMapping::LoadAt(0x8000_0000))?;
        let bp = dbg.hardware_breakpoint_symbol("checkpoint")?;
        let stop = dbg.run_to_breakpoint(&bp, Duration::from_secs(5))?;
        assert_eq!(stop.pc, Some(0x8000_0004));
        assert_eq!(dbg.read_register("s0")?, 42);
        assert_eq!(dbg.read_physical(0x8000_0000, 8)?, firmware);
        assert_eq!(dbg.read_virtual(0x8000_0000, 8)?, firmware);
        assert_eq!(dbg.translate(0, 0x8000_0000)?.physical_address, 0x8000_0000);
        dbg.remove_breakpoint(&bp)?;
        dbg.resume()?;
        dbg.interrupt()?;
        Ok(())
    })?;
    let logs = vm.artifacts().to_owned();
    assert!(vm.save_diagnostics()?.is_file());
    vm.shutdown()?;
    assert!(logs.join("gdb-mi.log").is_file());
    assert!(logs.join("serial.log").is_file());
    Ok(())
}

// Minimal RV64 ELF with a zero-linked .text and a checkpoint symbol at offset four.
// This keeps the integration test independent of cross-compilation tools.
fn symbol_elf(code: &[u8]) -> Vec<u8> {
    fn put(bytes: &mut [u8], offset: usize, value: u64, size: usize) {
        bytes[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
    }
    let mut elf = vec![0; 0x1240];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    for (offset, value, size) in [
        (16, 2, 2),
        (18, 243, 2),
        (20, 1, 4),
        (32, 64, 8),
        (40, 0x1100, 8),
        (52, 64, 2),
        (54, 56, 2),
        (56, 1, 2),
        (58, 64, 2),
        (60, 5, 2),
        (62, 4, 2),
    ] {
        put(&mut elf, offset, value, size);
    }
    // PT_LOAD, executable/readable; ELF addresses start at zero.
    for (offset, value, size) in [
        (64, 1, 4),
        (68, 5, 4),
        (72, 0x1000, 8),
        (96, code.len() as u64, 8),
        (104, code.len() as u64, 8),
        (112, 4096, 8),
    ] {
        put(&mut elf, offset, value, size);
    }
    elf[0x1000..0x1000 + code.len()].copy_from_slice(code);
    elf[0x1040..0x104c].copy_from_slice(b"\0checkpoint\0");
    let names = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
    elf[0x1060..0x1060 + names.len()].copy_from_slice(names);
    let symbol = 0x1010 + 24;
    put(&mut elf, symbol, 1, 4); // name
    put(&mut elf, symbol + 4, 0x12, 1); // global function
    put(&mut elf, symbol + 6, 1, 2); // .text
    put(&mut elf, symbol + 8, 4, 8); // checkpoint address
    put(&mut elf, symbol + 16, 4, 8);
    for (index, name, ty, flags, offset, size, link, info, align, entry_size) in [
        (1, 1, 1, 6, 0x1000, code.len() as u64, 0, 0, 4, 0),
        (2, 7, 2, 0, 0x1010, 48, 3, 1, 8, 24),
        (3, 15, 3, 0, 0x1040, 12, 0, 0, 1, 0),
        (4, 23, 3, 0, 0x1060, names.len() as u64, 0, 0, 1, 0),
    ] {
        let section = 0x1100 + index * 64;
        for (field, value, width) in [
            (0, name, 4),
            (4, ty, 4),
            (8, flags, 8),
            (24, offset, 8),
            (32, size, 8),
            (40, link, 4),
            (44, info, 4),
            (48, align, 8),
            (56, entry_size, 8),
        ] {
            put(&mut elf, section + field, value, width);
        }
    }
    elf
}

#[test]
#[ignore = "requires qemu-system-riscv64 and gdb-multiarch"]
fn real_high_mapping_trap_entry_return_and_uart() -> Result<()> {
    use tau_builder::debugger::riscv::PagingMode;
    const HIGH: u64 = 0xffff_ffc0_0000_0000;
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("supervisor.bin"), supervisor_fixture())?;
    let mut config = VmConfig::new(root.path());
    config.firmware = "supervisor.bin".into();
    config.memory_mib = 128;
    config.harts = 1;
    let mut vm = TestVm::spawn(config)?;
    vm.check(|vm| {
        let dbg = vm.debugger();
        let ready = dbg.hardware_breakpoint(HIGH + 0x9c)?;
        dbg.run_to_breakpoint(&ready, Duration::from_secs(5))?;
        dbg.remove_breakpoint(&ready)?;
        let satp = dbg.read_register("satp")?;
        let mapping = dbg.translate(satp, HIGH + 0x80)?;
        anyhow::ensure!(mapping.mode == PagingMode::Sv39);
        anyhow::ensure!(mapping.physical_address == 0x8000_0080);
        anyhow::ensure!(mapping.permissions.unwrap().execute);
        let physical = dbg.read_physical(0x8000_0080, 16)?;
        anyhow::ensure!(dbg.read_virtual(HIGH + 0x80, 16)? == physical);
        let trap_entry = dbg.trap_breakpoint(1)?;
        vm.serial().send(b"Z")?;
        let dbg = vm.debugger();
        let trap = dbg.run_until_interrupt(&trap_entry, 1, Duration::from_secs(5))?;
        anyhow::ensure!(trap.scause == (1 << 63) | 1);
        anyhow::ensure!(trap.sepc == HIGH + 0xa0);
        anyhow::ensure!(dbg.read_register("pc")? == HIGH + 0x100);
        dbg.remove_breakpoint(&trap_entry)?;
        let progress = dbg.hardware_breakpoint(HIGH + 0xc0)?;
        dbg.run_to_breakpoint(&progress, Duration::from_secs(5))?;
        anyhow::ensure!(dbg.read_register("s1")? == 43);
        anyhow::ensure!(dbg.read_register("s2")? == u64::from(b'Z'));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !vm.serial().tail().contains(&b'A') {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "missing captured UART output"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    })?;
    vm.shutdown()
}

// Small freestanding program that creates Sv39 mappings, enters S-mode at a high VA,
// raises/handles an SSIP trap, returns, then receives and sends a UART byte.
// It deliberately has no symbol for its trap entry and needs no assembler/OpenSBI.
fn supervisor_fixture() -> Vec<u8> {
    fn i(op: u32, funct: u32, rd: u32, rs: u32, imm: i32) -> u32 {
        ((imm as u32 & 0xfff) << 20) | (rs << 15) | (funct << 12) | (rd << 7) | op
    }
    fn csr(address: u32, funct: u32, rs: u32) -> u32 {
        (address << 20) | (rs << 15) | (funct << 12) | 0x73
    }
    let mut image = vec![0; 0x2000];
    let machine = [
        0x00001297,           // auipc t0, 1: root table at 0x80001000
        i(0x13, 5, 5, 5, 12), // srli t0,t0,12
        i(0x13, 0, 6, 0, 8),
        i(0x13, 1, 6, 6, 60),                                // Sv39 MODE
        (6 << 20) | (5 << 15) | (6 << 12) | (5 << 7) | 0x33, // or t0,t0,t1
        csr(0x180, 1, 5),                                    // csrw satp,t0
        0x12000073,                                          // sfence.vma
        i(0x13, 0, 5, 0, -1),
        csr(0x3b0, 1, 5), // pmpaddr0: all physical memory
        i(0x13, 0, 5, 0, 15),
        csr(0x3a0, 1, 5), // pmpcfg0: TOR/RWX
        i(0x13, 0, 5, 0, 2),
        csr(0x303, 1, 5), // mideleg: SSIP to supervisor
        i(0x13, 0, 5, 0, 1),
        i(0x13, 1, 5, 5, 11),
        csr(0x300, 1, 5), // mstatus.MPP = S
        i(0x13, 0, 5, 0, -1),
        i(0x13, 1, 5, 5, 38), // 0xffffffc000000000
        i(0x13, 0, 5, 5, 0x80),
        csr(0x341, 1, 5), // mepc = high supervisor entry
        0x30200073,       // mret
    ];
    let supervisor = [
        0x00000297,                    // 80: auipc t0,0
        i(0x13, 0, 5, 5, 0x80),        // 84: address of anonymous trap handler
        csr(0x105, 1, 5),              // 88: stvec
        i(0x13, 0, 8, 0, 42),          // 8c: s0 = 42
        i(0x13, 0, 6, 0, 2),           // 90: SSIE/SIE bit
        csr(0x104, 1, 6),              // 94: sie
        csr(0x100, 2, 6),              // 98: sstatus.SIE
        csr(0x144, 2, 6),              // 9c: set sip.SSIP
        i(0x13, 0, 9, 0, 43),          // a0: resumed progress, s1 = 43
        0x100003b7,                    // a4: lui t2,0x10000 (UART)
        i(0x03, 4, 28, 7, 5),          // a8: lbu t3,5(t2), line status
        i(0x13, 7, 28, 28, 1),         // ac: andi t3,t3,1
        0xfe0e0ce3,                    // b0: beq t3,zero,-8
        i(0x03, 4, 18, 7, 0),          // b4: lbu s2,0(t2), received byte
        i(0x13, 0, 28, 0, 65),         // b8: transmit 'A'
        (28 << 20) | (7 << 15) | 0x23, // bc: sb t3,0(t2)
        0x0000006f,                    // c0: jal zero,0
    ];
    for (base, instructions) in [
        (0, machine.as_slice()),
        (0x80, supervisor.as_slice()),
        (0x100, [csr(0x144, 3, 6), 0x10200073].as_slice()),
    ] {
        for (index, instruction) in instructions.iter().enumerate() {
            image[base + index * 4..base + index * 4 + 4]
                .copy_from_slice(&instruction.to_le_bytes());
        }
    }
    // One-GiB leaves: low MMIO identity mapping, RAM identity mapping, high supervisor.
    for (index, physical) in [(0, 0u64), (2, 0x80000000), (256, 0x80000000)] {
        let pte = ((physical >> 12) << 10) | 0xcf;
        image[0x1000 + index * 8..0x1008 + index * 8].copy_from_slice(&pte.to_le_bytes());
    }
    image
}
