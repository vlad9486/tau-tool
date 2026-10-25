//! Compile the supervisor's portable wait validation and parking transitions on
//! the host, keeping host-only fixtures outside the RISC-V OS workspace.
use std::{fs, path::PathBuf, process::Command};
use anyhow::{Context, Result, ensure};

#[test]
#[ignore = "requires OS sources and rustc; optionally set TAU_TEST_PROJECT"]
fn wait_validation_and_parking_model() -> Result<()> {
    let root = std::env::var_os("TAU_TEST_PROJECT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tau"))
        .canonicalize()?;
    let directory = tempfile::tempdir()?;
    let matter_module = fs::read_to_string(root.join("2-S/src/matter/module.rs"))?;
    let (_, wait_table_def) = matter_module
        .split_once("pub struct WaitTable {")
        .context("WaitTable definition")?;
    let (wait_table_def, _) = wait_table_def
        .split_once("impl ModuleDescriptor")
        .context("ModuleDescriptor impl")?;
    let (_, wait_record_and_impl) = matter_module
        .split_once("pub struct WaitRecord {")
        .context("WaitRecord definition")?;
    let (wait_record_and_impl, _) = wait_record_and_impl
        .split_once("pub fn create_module")
        .context("create_module fn")?;
    let waiting_path = directory.path().join("waiting.rs");
    fs::write(
        &waiting_path,
        format!(
            "use core::{{ptr, sync::atomic::{{AtomicU8, Ordering}}}};\n\
             use tau::{{ModuleError, WaitError}};\n\
             use super::{{state::Context, space::{{self, Root, WindowAccess, PAGE}}}};\n\
             pub struct WaitTable {{\n{wait_table_def}\n\
             pub struct WaitRecord {{\n{wait_record_and_impl}"
        ),
    )?;

    let mut harness = include_str!("fixtures/scheduling.rs").to_owned();
    harness = harness.replace("\"@COMMON@\"", &format!("{:?}", root.join("tau/src/common.rs")));
    harness = harness.replace("\"@ERROR@\"", &format!("{:?}", root.join("tau/src/error.rs")));
    harness = harness.replace("\"@WAITING@\"", &format!("{:?}", waiting_path));
    harness = harness.replace(
        "\"@THREAD@\"",
        &format!("{:?}", root.join("2-S/src/scheduler/thread.rs")),
    );
    let source = directory.path().join("scheduling.rs");
    let binary = directory.path().join("scheduling-tests");
    fs::write(&source, harness)?;
    let output = Command::new("rustc")
        .args(["--edition=2024", "--test"])
        .arg(source)
        .arg("-o")
        .arg(&binary)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(binary).arg("--nocapture").output()?;
    ensure!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}
