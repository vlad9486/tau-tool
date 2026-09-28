//! OS integration scenarios, driven from the host against the system-test payload.
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use crate::{
    build::Build,
    debugger::{Breakpoint, ElfImage, Gdb, SymbolMapping, TestVm, VmConfig, riscv},
};

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
    if case.id == 1 {
        ensure!(gdb.read_register("a1")? == hart, "boot hart changed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(selected(None).len(), 4);
        assert_eq!(selected(Some("timer"))[0].id, 3);
        assert!(selected(Some("timre")).is_empty());
    }

    // Opt-in regression against the actual payload. Run tau-builder test first;
    // this deliberately sends an invalid scenario and withholds a completion
    // breakpoint, ensuring neither guest failure nor a hang can count as a pass.
    #[test]
    #[ignore = "requires TAU_TEST_PROJECT with built test artifacts, QEMU and GDB"]
    fn payload_failure_and_timeout_capture_diagnostics() -> Result<()> {
        let root =
            PathBuf::from(std::env::var_os("TAU_TEST_PROJECT").context(
                "set TAU_TEST_PROJECT to the OS checkout after running tau-builder test",
            )?)
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
        ensure!(error.is::<crate::debugger::Timeout>(), "{error:#}");
        let diagnostics = fs::read_to_string(vm.artifacts().join("diagnostics.txt"))?;
        ensure!(
            diagnostics.contains("pc:"),
            "missing stopped-hart diagnostics"
        );
        vm.shutdown()
    }
}
