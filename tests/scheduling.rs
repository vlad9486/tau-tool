//! Compile the supervisor's portable wait validation and parking transitions on
//! the host, keeping host-only fixtures outside the RISC-V OS workspace.
use std::{fs, path::PathBuf, process::Command};
use anyhow::{Result, ensure};

#[test]
#[ignore = "requires OS sources and rustc; optionally set TAU_TEST_PROJECT"]
fn wait_validation_and_parking_model() -> Result<()> {
    let root = std::env::var_os("TAU_TEST_PROJECT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tau"))
        .canonicalize()?;
    let directory = tempfile::tempdir()?;
    let mut harness = include_str!("fixtures/scheduling.rs").to_owned();
    for (marker, path) in [
        ("\"@COMMON@\"", "tau/src/common.rs"),
        ("\"@WAITING@\"", "2-S/src/waiting.rs"),
        ("\"@THREAD@\"", "2-S/src/scheduler/thread.rs"),
    ] {
        harness = harness.replace(marker, &format!("{:?}", root.join(path)));
    }
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
