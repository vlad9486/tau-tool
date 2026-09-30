//! Exercise the OS's allocation-free writer on the host without making the
//! host tool depend on the RISC-V workspace or building host code in that repo.
use std::{fs, path::PathBuf, process::Command};
use anyhow::{Context, Result, ensure};

#[test]
#[ignore = "requires OS sources, rustc and dtc; optionally set TAU_TEST_PROJECT"]
fn boot_dtb_writer() -> Result<()> {
    let root = std::env::var_os("TAU_TEST_PROJECT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tau"))
        .canonicalize()?;
    let directory = tempfile::tempdir()?;
    let harness = directory.path().join("tests.rs");
    let loader = fs::read_to_string(root.join("2-S/src/bin/loader.rs"))?;
    let (_, writer) = loader
        .split_once("\nmod boot_dtb {")
        .context("loader's boot_dtb module")?;
    let layout = fs::read_to_string(root.join("2-S/src/layout.rs"))?;
    let (layout, _) = layout
        .split_once("include!(")
        .context("static module layout declaration")?;
    fs::write(
        &harness,
        format!(
            "extern crate self as supervisor;\npub mod layout {{ {layout} }}\n\
             mod boot_dtb {{ {writer}\nuse boot_dtb as writer;\n{}",
            include_str!("fixtures/dtb_writer.rs")
        ),
    )?;
    let binary = directory.path().join("dtb-tests");
    let output = Command::new("rustc")
        .args(["--edition=2024", "--test"])
        .arg(&harness)
        .arg("-o")
        .arg(&binary)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(binary)
        .arg("--nocapture")
        .env("TAU_DTB_TEST_DIR", directory.path())
        .env("TAU_TEST_PROJECT", root)
        .output()?;
    ensure!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}
