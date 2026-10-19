//! OS integration scenarios, driven from the host against the system-test payload.
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use super::{
    build::Build,
    debugger::{Breakpoint, ElfImage, Gdb, SymbolMapping, TestVm, VmConfig, riscv},
};

#[cfg(test)]
use super::debugger::Timeout;

const MAGIC: u64 = 0x5441_5554_4553_5431;
const VERSION: u64 = 1;

#[derive(Args)]
pub struct TestArgs {
    /// Run cases whose names contain this string.
    pub filter: Option<String>,
    /// List matching cases without building or starting QEMU.
    #[arg(long)]
    pub list: bool,
    #[arg(long, default_value = ".")]
    pub project_root: PathBuf,
    #[arg(long, default_value = "gdb-multiarch")]
    pub gdb: PathBuf,
    #[arg(long, default_value = "qemu-system-riscv64")]
    pub qemu: PathBuf,
    /// Execution deadline for each case, including boot (seconds).
    #[arg(long, default_value = "30", value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout: u64,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    id: u64,
    name: &'static str,
}

const CASES: &[Case] = &[
    Case { id: 7, name: "scheduling" },
    Case { id: 8, name: "spawn-rollback" },
    Case {
        id: 1,
        name: "boot",
    },
    Case {
        id: 2,
        name: "mapping",
    },
    Case {
        id: 3,
        name: "timer",
    },
    Case {
        id: 5,
        name: "invocation",
    },
    Case {
        id: 6,
        name: "graph",
    },
    Case {
        id: 4,
        name: "fault",
    },
];

fn selected(filter: Option<&str>) -> Vec<Case> {
    CASES
        .iter()
        .copied()
        .filter(|c| filter.is_none_or(|s| c.name.contains(s)))
        .collect()
}

pub fn run(args: TestArgs) -> Result<()> {
    let cases = selected(args.filter.as_deref());
    ensure!(
        !cases.is_empty(),
        "no test matches {:?}; use --list",
        args.filter
    );
    if args.list {
        for case in cases {
            println!("{}", case.name);
        }
        return Ok(());
    }
    let root = fs::canonicalize(&args.project_root).context("resolve OS project root")?;
    ensure!(
        root.join("3-U/test/Cargo.toml").is_file(),
        "missing system-test package in {}",
        root.display()
    );
    // Fail before compiling when a required host executable is missing.
    for program in [&args.gdb, &args.qemu] {
        let status = std::process::Command::new(program)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .with_context(|| {
                format!(
                    "start {}; install it or set --gdb/--qemu",
                    program.display()
                )
            })?;
        ensure!(status.success(), "{} --version failed", program.display());
    }
    let artifacts = root.join("target/tau-test");
    fs::create_dir_all(&artifacts)?;
    // Serialize test commands until all VMs exit. Ordinary build commands do
    // not take this lock and should run separately from tests.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(artifacts.join("build.lock"))?;
    println!("Waiting for test build lock...");
    lock.lock()?;
    let build = Build::tests(&root);
    build.build_payload()?;
    let firmware = build.build_qemu()?;
    let supervisor = ElfImage::read(build.binaries().join("supervisor"))?;
    let payload = ElfImage::read(build.binaries().join("system-test"))?;
    let suite = tempfile::Builder::new()
        .prefix("suite-")
        .tempdir_in(artifacts)?
        .keep();
    println!("Test artifacts: {}", suite.display());
    let mut results = Vec::new();
    let mut failures = 0;
    for case in &cases {
        let start = Instant::now();
        let mut config = VmConfig::new(&root);
        config.firmware = firmware.clone();
        config.gdb = args.gdb.clone();
        config.qemu = args.qemu.clone();
        config.artifacts_root = suite.join(case.name);
        let result = (|| -> Result<()> {
            let mut vm = TestVm::spawn(config)?;
            vm.check(|vm| {
                exercise(
                    vm.debugger(),
                    &supervisor,
                    &payload,
                    *case,
                    Duration::from_secs(args.timeout),
                )
            })?;
            vm.shutdown()
        })();
        let error = result.err().map(|e| format!("{e:#}"));
        if let Some(error) = &error {
            failures += 1;
            eprintln!("FAIL {}: {error}", case.name);
        } else {
            println!("PASS {} ({:.2}s)", case.name, start.elapsed().as_secs_f64());
        }
        results.push(serde_json::json!({
            "name": case.name, "passed": error.is_none(), "error": error,
            "seconds": start.elapsed().as_secs_f64(), "artifacts": suite.join(case.name),
        }));
        fs::write(
            suite.join("results.json"),
            serde_json::to_vec_pretty(&results)?,
        )?;
    }
    println!(
        "{} passed; {failures} failed; results: {}",
        cases.len() - failures,
        suite.join("results.json").display()
    );
    ensure!(failures == 0, "OS tests failed");
    Ok(())
}

fn breakpoint(gdb: &mut Gdb, image: &ElfImage, symbol: &str) -> Result<Breakpoint> {
    gdb.hardware_breakpoint(image.symbol_address(symbol, SymbolMapping::Linked)?)
}

fn until(
    gdb: &mut Gdb,
    expected: &Breakpoint,
    failed: &Breakpoint,
    deadline: Instant,
) -> Result<()> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .context("test execution deadline exceeded")?;
    gdb.resume()?;
    let stop = gdb.wait_for_stop(timeout)?;
    if stop.hit(failed) {
        bail!(
            "payload failure: case={} code={} value={:#x}",
            gdb.read_register("a0")?,
            gdb.read_register("a1")?,
            gdb.read_register("a2")?
        );
    }
    ensure!(stop.hit(expected), "unexpected stop: {stop:?}");
    Ok(())
}

fn check_case(gdb: &mut Gdb, case: Case) -> Result<()> {
    ensure!(
        gdb.read_register("a0")? == case.id,
        "checkpoint has wrong scenario ID"
    );
    Ok(())
}

fn validate_control(bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() == 24, "invalid test control block size");
    let word = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    ensure!(word(0) == MAGIC, "invalid test payload magic");
    ensure!(
        word(8) == VERSION,
        "unsupported test protocol version {} (expected {VERSION})",
        word(8)
    );
    ensure!(
        word(16) == 0,
        "test payload already has a selected scenario"
    );
    Ok(())
}

// QEMU suite uses 4 GiB (1M frames). Each allocator tree has eight u16
// counters in a 64-byte cache line, following 2048 64-byte frame bitmaps.
// Compare free-frame counts at quiescent guest checkpoints, not page addresses.
fn free_frames(gdb: &mut Gdb, satp: u64) -> Result<u64> {
    let tables = gdb
        .translate(satp, 0xffff_ffc0_006e_0000 + 2048 * 64)?
        .physical_address;
    let data = gdb.read_physical(tables, 256 * 64)?;
    Ok(data
        .as_chunks::<64>()
        .0
        .iter()
        .flat_map(|tree| tree[..16].as_chunks::<2>().0.iter())
        .map(|cell| u16::from_le_bytes(*cell))
        .filter(|&count| count != u16::MAX)
        .map(u64::from)
        .sum())
}

fn physical_word(gdb: &mut Gdb, address: u64) -> Result<u64> {
    Ok(u64::from_le_bytes(
        gdb.read_physical(address, 8)?.try_into().unwrap(),
    ))
}

fn dependency_root(gdb: &mut Gdb, root: u64, slot: u64) -> Result<u64> {
    // repr(C) ModuleContext: 32 registers, five header words, then roots.
    let address = gdb
        .translate(root, 0xffff_ffc0_0021_0000 + (37 + slot) * 8)?
        .physical_address;
    physical_word(gdb, address)
}

fn exercise(
    gdb: &mut Gdb,
    supervisor: &ElfImage,
    payload: &ElfImage,
    case: Case,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("test timeout is too large")?;
    gdb.add_symbols(supervisor, SymbolMapping::Linked)?;
    gdb.add_symbols(payload, SymbolMapping::Linked)?;
    let failed = breakpoint(gdb, payload, "tau_test_failed")?;
    let ready = breakpoint(gdb, payload, "tau_test_ready")?;
    let passed = breakpoint(gdb, payload, "tau_test_passed")?;
    until(gdb, &ready, &failed, deadline).context("reach U-mode readiness")?;
    gdb.remove_breakpoint(&ready)?;
    let hart = gdb.read_register("a0")?;
    ensure!(
        gdb.read_register("priv")? == 0,
        "payload is not executing in U-mode"
    );
    let satp = gdb.read_register("satp")?;
    let pc = gdb.read_register("pc")?;
    let mapping = gdb.translate(satp, pc)?;
    ensure!(
        mapping.permissions.is_some_and(|p| p.user && p.execute),
        "payload code must have a user executable mapping"
    );
    let control = payload.symbol_address("TAU_TEST_CONTROL", SymbolMapping::Linked)?;
    validate_control(&gdb.read_virtual(control, 24)?)?;
    gdb.write_virtual(control + 16, &case.id.to_le_bytes())?;
    ensure!(
        gdb.read_virtual(control + 16, 8)? == case.id.to_le_bytes(),
        "scenario selection write did not stick"
    );

    if case.id != 1 {
        let progress = breakpoint(gdb, payload, "tau_test_progress")?;
        until(gdb, &progress, &failed, deadline).context("reach scenario progress")?;
        gdb.remove_breakpoint(&progress)?;
        check_case(gdb, case)?;
        let value = gdb.read_register("a1")?;
        let extra = gdb.read_register("a2")?;
        match case.id {
            2 => {
                ensure!(
                    value == 0x10_0000_0000 && extra == 2,
                    "unexpected mapping test range"
                );
                let satp = gdb.read_register("satp")?;
                let first = gdb.translate(satp, value)?;
                let second = gdb.translate(satp, value + 0x1000)?;
                for mapping in [&first, &second] {
                    ensure!(
                        mapping
                            .permissions
                            .is_some_and(|p| p.user && p.read && p.write && !p.execute),
                        "wrong mapping permissions: {mapping:?}"
                    );
                }
                ensure!(
                    first.physical_address != second.physical_address,
                    "allocated pages alias"
                );
                for offset in [0, 0xff8, 0x1000, 0x1ff8] {
                    let actual = gdb.read_virtual(value + offset, 8)?;
                    ensure!(
                        actual == (0x1234_5678_9abc_def0u64 ^ offset).to_le_bytes(),
                        "mapped memory mismatch at {offset:#x}"
                    );
                }
            }
            3 => ensure!(
                extra >= value && value > 0,
                "timer returned before its deadline"
            ),
            8 => {
                ensure!(extra == 0, "wrong spawn rollback checkpoint");
                let before = free_frames(gdb, satp)?;
                let hart_mmu = gdb.translate(satp, 0xffff_ffc0_0000_6000)?.physical_address;
                let mask = gdb.read_physical(hart_mmu, 2)?;
                let next = gdb.read_physical(hart_mmu + 8, 8)?;
                let maximum = u16::from_le_bytes(mask.clone().try_into().unwrap()).max(1);
                gdb.write_physical(hart_mmu, &maximum.to_le_bytes())?;
                gdb.write_physical(hart_mmu + 8, &(u64::from(maximum) + 1).to_le_bytes())?;
                let checkpoint = breakpoint(gdb, payload, "tau_test_progress")?;
                until(gdb, &checkpoint, &failed, deadline).context("reject spawn without ASID")?;
                ensure!(gdb.read_register("a2")? == 1, "wrong post-failure checkpoint");
                ensure!(gdb.read_register("satp")? == satp, "failed spawn switched roots");
                ensure!(free_frames(gdb, satp)? == before, "failed spawn leaked pages");
                ensure!(physical_word(gdb, hart_mmu + 8)? == u64::from(maximum) + 1,
                    "failed spawn consumed an ASID");
                gdb.write_physical(hart_mmu, &mask)?;
                gdb.write_physical(hart_mmu + 8, &next)?;
                gdb.remove_breakpoint(&checkpoint)?;
            }
            7 => {
                ensure!(value & 63 == 0 && extra == 8, "invalid wait table checkpoint");
                let flags = gdb.translate(satp, value)?.permissions.context("missing wait table")?;
                ensure!(flags.user && flags.read && flags.write && !flags.execute,
                    "wait table has incorrect permissions");
            }
            6 => {
                ensure!(extra == 0, "wrong graph construction checkpoint");
                let snapshot = gdb.read_virtual(value, 9 * 8)?;
                let words: Vec<u64> = snapshot
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|word| u64::from_le_bytes(*word))
                    .collect();
                let descriptors: Vec<u64> = words[..4].iter().map(|ppn| ppn << 12).collect();
                let slots = &words[4..];
                ensure!(
                    slots.iter().all(|slot| *slot > 0 && *slot < 0x800),
                    "invalid graph slots"
                );
                for (index, descriptor) in descriptors.iter().enumerate() {
                    ensure!(
                        *descriptor != 0 && !descriptors[..index].contains(descriptor),
                        "duplicate module handle"
                    );
                }
                let records: Vec<Vec<u8>> = descriptors
                    .iter()
                    .map(|descriptor| gdb.read_physical(*descriptor, 40))
                    .collect::<Result<_>>()?;
                let field = |module: usize, offset: usize| {
                    u64::from_le_bytes(records[module][offset..offset + 8].try_into().unwrap())
                };
                let caller = gdb.read_register("satp")?;
                for (slot, descriptor) in [(slots[0], descriptors[0]), (slots[1], descriptors[1])] {
                    let address = gdb
                        .translate(caller, 0xffff_ffc0_0022_0008 + slot * 8)?
                        .physical_address;
                    ensure!(
                        physical_word(gdb, address)? == descriptor | (1 << 63),
                        "wrong root binding"
                    );
                    ensure!(
                        dependency_root(gdb, caller, slot)? == 0,
                        "graph construction created a context"
                    );
                }
                for (parent, slot, child) in [(0, slots[2], 2), (1, slots[3], 2), (2, slots[4], 3)]
                {
                    ensure!(
                        physical_word(gdb, field(parent, 32) + 8 + slot * 8)?
                            == descriptors[child] | (1 << 63),
                        "wrong graph binding"
                    );
                }
                ensure!(
                    field(0, 0) != field(1, 0),
                    "fresh preparations shared ELF backing"
                );
                ensure!(
                    field(0, 32) != field(1, 32),
                    "fresh preparations shared module tables"
                );

                let warm = breakpoint(gdb, payload, "tau_test_progress")?;
                until(gdb, &warm, &failed, deadline).context("walk both graph paths")?;
                check_case(gdb, case)?;
                ensure!(
                    gdb.read_register("a1")? == value && gdb.read_register("a2")? == 1,
                    "wrong graph warm checkpoint"
                );
                gdb.remove_breakpoint(&warm)?;
                let a = dependency_root(gdb, caller, slots[0])?;
                let b = dependency_root(gdb, caller, slots[1])?;
                let service_a = dependency_root(gdb, a, slots[2])?;
                let service_b = dependency_root(gdb, b, slots[3])?;
                let leaf_a = dependency_root(gdb, service_a, slots[4])?;
                let leaf_b = dependency_root(gdb, service_b, slots[4])?;
                let contexts = [a, b, service_a, service_b, leaf_a, leaf_b];
                for (index, root) in contexts.iter().enumerate() {
                    ensure!(
                        *root != 0 && !contexts[..index].contains(root),
                        "graph paths shared a private context"
                    );
                }
                for (root, module) in [
                    (a, 0),
                    (b, 1),
                    (service_a, 2),
                    (service_b, 2),
                    (leaf_a, 3),
                    (leaf_b, 3),
                ] {
                    let tables = gdb.translate(root, 0xffff_ffc0_0022_0000)?;
                    ensure!(
                        tables.physical_address == field(module, 32),
                        "context has wrong shared tables"
                    );
                    ensure!(
                        tables.permissions.is_some_and(|p| !p.user),
                        "module tables accessible to userspace"
                    );
                    let metadata = gdb.translate(root, 0xffff_ffc0_0021_0000)?.physical_address;
                    ensure!(
                        physical_word(gdb, metadata + 35 * 8)? == root,
                        "wrong context root"
                    );
                    ensure!(
                        physical_word(gdb, metadata + 33 * 8)? == 0,
                        "idle graph context retains caller"
                    );
                }
                for (first, second, module) in [(service_a, service_b, 2), (leaf_a, leaf_b, 3)] {
                    ensure!(
                        gdb.translate(first, field(module, 16))?.physical_address
                            == gdb.translate(second, field(module, 16))?.physical_address,
                        "shared graph module has different ELF backing"
                    );
                    ensure!(
                        gdb.translate(first, u64::MAX - 7)?.physical_address
                            != gdb.translate(second, u64::MAX - 7)?.physical_address,
                        "graph paths share private stacks"
                    );
                }
                let available = free_frames(gdb, caller)?;
                until(gdb, &passed, &failed, deadline)
                    .context("complete graph walks and slot exhaustion")?;
                check_case(gdb, case)?;
                ensure!(gdb.read_register("a1")? == 128, "wrong graph walk count");
                ensure!(
                    gdb.read_register("satp")? == caller,
                    "graph walk did not restore root"
                );
                ensure!(
                    free_frames(gdb, caller)? == available,
                    "warm graph walks or failed preparation leaked pages"
                );
                for (parent, slot, expected) in [
                    (caller, slots[0], a),
                    (caller, slots[1], b),
                    (a, slots[2], service_a),
                    (b, slots[3], service_b),
                    (service_a, slots[4], leaf_a),
                    (service_b, slots[4], leaf_b),
                ] {
                    ensure!(
                        dependency_root(gdb, parent, slot)? == expected,
                        "warm graph walk replaced a context"
                    );
                }
                ensure!(
                    physical_word(gdb, field(1, 32) + 8)? == 0x800,
                    "exhausted table has wrong free-list head"
                );
                ensure!(
                    physical_word(gdb, field(1, 32) + 8 + 0x7ff * 8)? == descriptors[2] | (1 << 63),
                    "last binding did not reuse service descriptor"
                );
                return Ok(());
            }
            5 => {
                ensure!(
                    value > 0 && value < 0x800 && extra > 0 && extra < 0x800 && value != extra,
                    "invalid prepared dependency slots"
                );
                // repr(C) ModuleTables: two bools, padding, then atomic slots.
                let caller_root = gdb.read_register("satp")?;
                let slot_address = gdb
                    .translate(caller_root, 0xffff_ffc0_0022_0000 + 8 + value * 8)?
                    .physical_address;
                let cell = gdb.read_physical(slot_address, 8)?;
                let descriptor = u64::from_le_bytes(cell.try_into().unwrap()) & !(1 << 63);
                ensure!(
                    descriptor != 0 && descriptor & 0xfff == 0,
                    "invalid descriptor"
                );
                // repr(C) immutable PreparedModule: stem, root index, entry, stack, tables.
                let record = gdb.read_physical(descriptor, 40)?;
                let entry = u64::from_le_bytes(record[16..24].try_into().unwrap());
                let sibling_slot = gdb
                    .translate(caller_root, 0xffff_ffc0_0022_0000 + 8 + extra * 8)?
                    .physical_address;
                let sibling_descriptor =
                    u64::from_le_bytes(gdb.read_physical(sibling_slot, 8)?.try_into().unwrap())
                        & !(1 << 63);
                let caller = gdb.read_register("satp")?;
                let available_before = free_frames(gdb, caller)?;
                let context_va = 0xffff_ffc0_0021_0000;
                // repr(C) ModuleContext: 32 registers and five header words before cached roots.
                let roots_va = context_va + 37 * 8;
                let hart_mmu = gdb
                    .translate(caller, 0xffff_ffc0_0000_6000)?
                    .physical_address;
                let mask = u16::from_le_bytes(gdb.read_physical(hart_mmu, 2)?.try_into().unwrap());
                let next_tag = gdb.read_physical(hart_mmu + 8, 8)?;
                let exhausted_mask = mask.max(1);
                gdb.write_physical(hart_mmu, &exhausted_mask.to_le_bytes())?;
                gdb.write_physical(hart_mmu + 8, &(u64::from(exhausted_mask) + 1).to_le_bytes())?;
                let rollback = breakpoint(gdb, payload, "tau_test_progress")?;
                until(gdb, &rollback, &failed, deadline).context("reject exhausted ASIDs")?;
                ensure!(gdb.read_register("a2")? == 0, "wrong rollback checkpoint");
                ensure!(
                    gdb.read_register("satp")? == caller,
                    "failed invocation switched roots"
                );
                ensure!(
                    free_frames(gdb, caller)? == available_before,
                    "failed context construction leaked pages"
                );
                let empty_slot = gdb
                    .translate(caller, roots_va + value * 8)?
                    .physical_address;
                ensure!(
                    gdb.read_physical(empty_slot, 8)? == 0u64.to_le_bytes(),
                    "failed construction published a root"
                );
                gdb.write_physical(hart_mmu, &mask.to_le_bytes())?;
                gdb.write_physical(hart_mmu + 8, &next_tag)?;
                gdb.remove_breakpoint(&rollback)?;
                let thread_va = 0xffff_ffc0_0020_0000;
                let caller_context = gdb.translate(caller, context_va)?.physical_address;
                let caller_thread = gdb.translate(caller, thread_va)?.physical_address;
                let caller_stack = gdb
                    .translate(caller, 0xffff_ffc0_001f_fff0)?
                    .physical_address;
                let caller_code = gdb.translate(caller, entry)?.physical_address;
                let destination = gdb.hardware_breakpoint(entry)?;
                loop {
                    until(gdb, &destination, &failed, deadline)
                        .context("enter separate dummy-test address space")?;
                    if gdb.read_register("satp")? != caller {
                        break;
                    }
                }
                let callee = gdb.read_register("satp")?;
                let bound_slot = gdb
                    .translate(callee, 0xffff_ffc0_0022_0010)?
                    .physical_address;
                ensure!(
                    gdb.read_physical(bound_slot, 8)?
                        == (sibling_descriptor | (1 << 63)).to_le_bytes(),
                    "loader binding does not refer to the sibling descriptor"
                );
                if mask != 0 {
                    ensure!((callee >> 44) & 0xffff == 1, "callee lacks distinct ASID");
                    ensure!((caller >> 44) & 0xffff == 0, "bootstrap tag changed");
                }

                ensure!(gdb.read_register("priv")? == 0, "callee is not in U-mode");
                ensure!(
                    gdb.translate(callee, entry)?
                        .permissions
                        .is_some_and(|p| p.user && p.execute && !p.write),
                    "callee entry permissions"
                );
                ensure!(
                    gdb.translate(callee, entry)?.physical_address != caller_code,
                    "callee inherited caller ELF backing"
                );
                ensure!(
                    gdb.translate(callee, context_va)?.physical_address != caller_context,
                    "context pages are shared"
                );
                ensure!(
                    gdb.translate(callee, thread_va)?.physical_address == caller_thread,
                    "invoke created another thread"
                );
                ensure!(
                    gdb.translate(callee, 0xffff_ffc0_001f_fff0)?
                        .physical_address
                        == caller_stack,
                    "per-hart supervisor stack changed"
                );
                ensure!(
                    gdb.translate(callee, u64::MAX - 15)?
                        .permissions
                        .is_some_and(|p| p.user && p.read && p.write && !p.execute),
                    "missing private user stack"
                );
                ensure!(
                    gdb.translate(callee, context_va)?
                        .permissions
                        .is_some_and(|p| !p.user),
                    "context metadata is user accessible"
                );
                let fresh_registers = [
                    "ra", "sp", "gp", "tp", "t0", "t1", "t2", "t3", "t4", "t5", "t6", "s0", "s1",
                    "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "a6", "a7",
                ];
                for name in fresh_registers {
                    ensure!(gdb.read_register(name)? == 0, "callee inherited {name}");
                }
                let available_first = free_frames(gdb, caller)?;
                ensure!(
                    available_first < available_before,
                    "cold invocation allocated no context"
                );
                // Exercise a warm switch with a forced flush, then normal tagged switches.
                gdb.write_physical(hart_mmu + 2, &[1])?;
                loop {
                    until(gdb, &destination, &failed, deadline)?;
                    if gdb.read_register("satp")? != caller {
                        break;
                    }
                }
                ensure!(
                    gdb.read_register("satp")? == callee,
                    "warm invocation replaced its root or ASID"
                );
                ensure!(
                    free_frames(gdb, caller)? == available_first,
                    "warm invocation allocated pages"
                );
                for name in fresh_registers {
                    ensure!(
                        gdb.read_register(name)? == 0,
                        "warm invocation retained {name}"
                    );
                }
                gdb.write_physical(hart_mmu + 2, &[0])?;
                loop {
                    until(gdb, &destination, &failed, deadline)?;
                    if gdb.read_register("satp")? != caller {
                        break;
                    }
                }
                let sibling = gdb.read_register("satp")?;
                ensure!(
                    sibling != callee && sibling != caller,
                    "sibling dependency reused another context"
                );
                if mask != 0 {
                    ensure!(
                        (sibling >> 44) & 0xffff == 2,
                        "sibling reused an ASID at the same call depth"
                    );
                }
                let available_second = free_frames(gdb, caller)?;
                ensure!(
                    available_second < available_first,
                    "sibling context was not constructed lazily"
                );
                loop {
                    until(gdb, &destination, &failed, deadline)?;
                    if gdb.read_register("satp")? != caller {
                        break;
                    }
                }
                ensure!(
                    gdb.read_register("satp")? == sibling,
                    "sibling's warm invocation replaced its root"
                );
                ensure!(
                    free_frames(gdb, caller)? == available_second,
                    "sibling's warm invocation allocated pages"
                );
                gdb.remove_breakpoint(&destination)?;
                let warm = breakpoint(gdb, payload, "tau_test_progress")?;
                until(gdb, &warm, &failed, deadline)
                    .context("finish context construction and private mappings")?;
                check_case(gdb, case)?;
                ensure!(gdb.read_register("a2")? == 1, "wrong warm checkpoint");
                gdb.remove_breakpoint(&warm)?;
                let available_warm = free_frames(gdb, caller)?;
                let private_first = gdb.translate(callee, 0x1_0000_0000)?.physical_address;
                let private_second = gdb.translate(sibling, 0x1_0000_0000)?.physical_address;
                ensure!(
                    private_first != private_second,
                    "private mappings alias sibling context"
                );
                for (slot, root) in [(value, callee), (extra, sibling)] {
                    let cached = gdb.translate(caller, roots_va + slot * 8)?.physical_address;
                    ensure!(
                        gdb.read_physical(cached, 8)? == root.to_le_bytes(),
                        "caller did not retain dependency satp"
                    );
                    let own_root = gdb.translate(root, context_va + 35 * 8)?.physical_address;
                    ensure!(
                        gdb.read_physical(own_root, 8)? == root.to_le_bytes(),
                        "context does not store its own root"
                    );
                    let child_cache = gdb.translate(root, roots_va + extra * 8)?;
                    ensure!(
                        child_cache.permissions.is_some_and(|p| !p.user),
                        "dependency cache is user accessible"
                    );
                    ensure!(
                        gdb.read_physical(child_cache.physical_address, 8)? == 0u64.to_le_bytes(),
                        "dependency cache is shared with caller"
                    );
                }
                let nested_slot = gdb.translate(callee, roots_va + 8)?.physical_address;
                let nested =
                    u64::from_le_bytes(gdb.read_physical(nested_slot, 8)?.try_into().unwrap());
                ensure!(
                    nested != 0 && nested != sibling && nested != callee,
                    "different dependency paths shared a context"
                );
                if mask != 0 {
                    ensure!(
                        (nested >> 44) & 0xffff == 3,
                        "nested context lacks a stable unique ASID"
                    );
                }
                ensure!(
                    gdb.translate(nested, entry)?.physical_address
                        == gdb.translate(sibling, entry)?.physical_address,
                    "same module contexts do not share ELF backing"
                );
                ensure!(
                    gdb.translate(nested, context_va)?.physical_address
                        != gdb.translate(sibling, context_va)?.physical_address,
                    "nested metadata is shared"
                );
                ensure!(
                    gdb.translate(nested, 0xffff_ffc0_0022_0000)?
                        .physical_address
                        == gdb
                            .translate(sibling, 0xffff_ffc0_0022_0000)?
                            .physical_address,
                    "same module contexts do not share module tables"
                );
                until(gdb, &passed, &failed, deadline)
                    .context("complete repeated module invocations")?;
                check_case(gdb, case)?;
                ensure!(
                    gdb.read_register("satp")? == caller,
                    "return did not restore caller root"
                );
                ensure!(gdb.read_register("a1")? == 128, "wrong invocation count");
                ensure!(
                    free_frames(gdb, caller)? == available_warm,
                    "warm invocations or failed preparations changed retained page count"
                );
                ensure!(
                    gdb.translate(callee, 0x1_0000_0000)?.physical_address == private_first,
                    "return replaced private backing"
                );
                ensure!(
                    gdb.translate(sibling, 0x1_0000_0000)?.physical_address == private_second,
                    "return replaced sibling private backing"
                );
                ensure!(
                    gdb.read_physical(nested_slot, 8)? == nested.to_le_bytes(),
                    "nested warm invocation replaced its root"
                );
                for root in [callee, sibling, nested] {
                    let link = gdb.translate(root, context_va + 33 * 8)?.physical_address;
                    ensure!(
                        gdb.read_physical(link, 8)? == 0u64.to_le_bytes(),
                        "idle context retained a caller link"
                    );
                }
                return Ok(());
            }
            4 => {
                ensure!(value == 0x20_0000_0000, "unexpected fault address");
                let stvec = gdb.read_register("stvec")?;
                let trap = gdb.hardware_breakpoint(riscv::trap_address(stvec, None)?)?;
                until(gdb, &trap, &failed, deadline).context("reach expected fault")?;
                let snapshot = gdb.trap_snapshot()?;
                ensure!(
                    snapshot.scause == 13,
                    "expected load page fault: {snapshot:?}"
                );
                ensure!(snapshot.stval == value, "wrong fault address: {snapshot:?}");
                ensure!(
                    snapshot.sepc
                        == payload
                            .symbol_address("tau_test_fault_instruction", SymbolMapping::Linked)?,
                    "wrong faulting instruction: {snapshot:?}"
                );
                ensure!(
                    snapshot.sstatus & (1 << 8) == 0,
                    "fault did not originate in U-mode"
                );
                return Ok(());
            }
            _ => unreachable!(),
        }
    }
    until(gdb, &passed, &failed, deadline).context("reach success checkpoint")?;
    check_case(gdb, case)?;
    if case.id == 7 {
        ensure!(gdb.read_register("a1")? == 128, "wrong scheduling round count");
        ensure!(gdb.read_register("satp")? == satp, "scheduler did not restore initial context");
    }
    if case.id == 1 {
        ensure!(gdb.read_register("a1")? == hart, "boot hart changed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        breakpoint, exercise, fs, selected, until, validate_control, Build, Case, Context,
        Duration, ElfImage, Instant, PathBuf, Result, SymbolMapping, TestVm, Timeout, VmConfig,
        MAGIC, VERSION, ensure,
    };

    #[test]
    fn control_rejects_incompatible_or_reused_payloads() {
        let encode = |words: [u64; 3]| {
            words
                .into_iter()
                .flat_map(u64::to_le_bytes)
                .collect::<Vec<_>>()
        };
        assert!(validate_control(&encode([MAGIC, VERSION, 0])).is_ok());
        for words in [
            [0, VERSION, 0],
            [MAGIC, VERSION + 1, 0],
            [MAGIC, VERSION, 1],
        ] {
            assert!(validate_control(&encode(words)).is_err());
        }
        assert!(validate_control(&[]).is_err());
    }

    #[test]
    fn selection_does_not_silently_run_everything_on_a_typo() {
        assert_eq!(selected(None).len(), 8);
        assert_eq!(selected(Some("invocation"))[0].id, 5);
        assert_eq!(selected(Some("graph"))[0].id, 6);
        assert_eq!(selected(Some("timer"))[0].id, 3);
        assert!(selected(Some("timre")).is_empty());
    }

    // Opt-in regression against the actual payload. Run `tau test` first;
    // this deliberately sends an invalid scenario and withholds a completion
    // breakpoint, ensuring neither guest failure nor a hang can count as a pass.
    #[test]
    #[ignore = "requires TAU_TEST_PROJECT with built test artifacts, QEMU and GDB"]
    fn payload_failure_and_timeout_capture_diagnostics() -> Result<()> {
        let root = PathBuf::from(
            std::env::var_os("TAU_TEST_PROJECT")
                .context("set TAU_TEST_PROJECT to the OS checkout after running tau test")?,
        )
        .canonicalize()?;
        let build = Build::tests(&root);
        let lock = fs::OpenOptions::new()
            .write(true)
            .open(root.join("target/tau-test/build.lock"))?;
        lock.lock()?;
        let payload = ElfImage::read(build.binaries().join("system-test"))?;
        let supervisor = ElfImage::read(build.binaries().join("supervisor"))?;
        let mut config = VmConfig::new(&root);
        config.firmware = build.binaries().join("tau-qemu");
        config.artifacts_root = "target/tau-test/runner-regression".into();

        let mut vm = TestVm::spawn(config.clone())?;
        let error = vm
            .check(|vm| {
                exercise(
                    vm.debugger(),
                    &supervisor,
                    &payload,
                    Case {
                        id: 99,
                        name: "invalid",
                    },
                    Duration::from_secs(5),
                )
            })
            .unwrap_err();
        ensure!(
            format!("{error:#}").contains("payload failure: case=99 code=0 value=0x63"),
            "{error:#}"
        );
        ensure!(vm.artifacts().join("diagnostics.txt").is_file());
        vm.shutdown()?;

        let mut vm = TestVm::spawn(config)?;
        let error = vm
            .check(|vm| {
                let gdb = vm.debugger();
                gdb.add_symbols(&payload, SymbolMapping::Linked)?;
                let ready = breakpoint(gdb, &payload, "tau_test_ready")?;
                let failed = breakpoint(gdb, &payload, "tau_test_failed")?;
                until(
                    gdb,
                    &ready,
                    &failed,
                    Instant::now() + Duration::from_secs(5),
                )?;
                gdb.remove_breakpoint(&ready)?;
                let control = payload.symbol_address("TAU_TEST_CONTROL", SymbolMapping::Linked)?;
                gdb.write_virtual(control + 16, &1u64.to_le_bytes())?;
                let never = gdb.hardware_breakpoint(0x10_0000_0000)?;
                until(
                    gdb,
                    &never,
                    &failed,
                    Instant::now() + Duration::from_millis(50),
                )
            })
            .unwrap_err();
        ensure!(error.is::<Timeout>(), "{error:#}");
        let diagnostics = fs::read_to_string(vm.artifacts().join("diagnostics.txt"))?;
        ensure!(
            diagnostics.contains("pc:"),
            "missing stopped-hart diagnostics"
        );
        vm.shutdown()
    }
}
