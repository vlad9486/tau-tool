#![cfg(unix)]

use std::{fs, path::PathBuf, time::Duration};
use anyhow::{Context, Result, ensure};
use object::{Object, ObjectSegment};
use tau_tool::{
    debugger::{ElfImage, SymbolMapping, TestVm, VmConfig},
    modules::Modules,
};

/// Run after `tau build --qemu`. The default assumes sibling OS/tool checkouts.
#[test]
#[ignore = "requires a normal Tau QEMU image; optionally set TAU_TEST_PROJECT"]
fn normal_system_receives_module_dtb() -> Result<()> {
    let root = std::env::var_os("TAU_TEST_PROJECT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tau"))
        .canonicalize()?;
    let binaries = root.join("target/riscv64imac-unknown-none-elf/release");
    let image = ElfImage::read(binaries.join("system"))?;
    let bytes = fs::read(&image.path)?;
    let elf = object::File::parse(bytes.as_slice())?;
    // Manifest starts with the 24-byte ModuleId, followed by the entry address.
    let segment = elf
        .segments()
        .find(|s| image.entry >= s.address() && image.entry + 32 <= s.address() + s.size())
        .context("manifest segment")?;
    let offset = (image.entry - segment.address()) as usize + 24;
    let entry = u64::from_le_bytes(segment.data()?[offset..offset + 8].try_into()?);
    let mut vm = TestVm::spawn(VmConfig::new(&root))?;
    vm.check(|vm| {
        let dbg = vm.debugger();
        dbg.add_symbols(&image, SymbolMapping::Linked)?;
        let main = dbg.hardware_breakpoint(entry)?;
        dbg.run_to_breakpoint(&main, Duration::from_secs(10))?;
        ensure!(dbg.read_register("priv")? == 0);
        let dtb = dbg.read_register("a2")?;
        let pages = dbg.read_register("a3")?;
        ensure!(dtb != 0 && dtb & 0xfff == 0 && pages != 0);
        let header = dbg.read_physical(dtb, 40)?;
        ensure!(&header[..4] == b"\xd0\x0d\xfe\xed");
        let len = u32::from_be_bytes(header[4..8].try_into()?) as usize;
        ensure!(pages == (len as u64).div_ceil(4096));
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("boot.dtb");
        fs::write(&path, dbg.read_physical(dtb, len)?)?;
        let config = Modules::read(&root.join("2-S/modules.toml"))?;
        let firmware = ElfImage::read(binaries.join("tau-qemu"))?;
        let base = firmware.symbol_address("__PAYLOAD_START", SymbolMapping::Linked)?;
        let packed = fs::read(binaries.join("tau"))?;
        for module in config
            .module
            .iter()
            .filter(|m| !matches!(m.name.as_str(), "loader" | "supervisor"))
        {
            let address = base + ((module.offset as u64) << 12);
            let node = format!("/chosen/tau,modules/module@{address:x}");
            let name = std::process::Command::new("fdtget")
                .arg("-t")
                .arg("s")
                .arg(&path)
                .arg(&node)
                .arg("tau,name")
                .output()?;
            ensure!(
                name.status.success(),
                "fdtget module name: {}",
                String::from_utf8_lossy(&name.stderr)
            );
            ensure!(String::from_utf8(name.stdout)?.trim() == module.name);
            let reg = std::process::Command::new("fdtget")
                .args(["-t", "x"])
                .arg(&path)
                .arg(&node)
                .arg("reg")
                .output()?;
            ensure!(reg.status.success(), "fdtget module range");
            let cells: Vec<u64> = String::from_utf8(reg.stdout)?
                .split_whitespace()
                .map(|word| u64::from_str_radix(word, 16))
                .collect::<Result<_, _>>()?;
            ensure!(
                cells
                    == [
                        address >> 32,
                        address & 0xffff_ffff,
                        0,
                        (module.size as u64) << 12
                    ]
            );
            let len = module.size << 12;
            ensure!(
                dbg.read_physical(address, len)?
                    == packed[module.offset << 12..(module.offset + module.size) << 12]
            );
        }
        dbg.remove_breakpoint(&main)?;
        Ok(())
    })?;
    vm.shutdown()
}
