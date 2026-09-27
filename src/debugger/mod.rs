//! Host-side QEMU/GDB API for bare-metal RISC-V tests. No guest agent or ramdisk.
//! All relative artifact paths resolve against VmConfig::project_root.

mod gdb;
mod mi;
mod qmp;
pub mod riscv;
mod serial;
mod transport;

pub use gdb::{Breakpoint, ElfImage, Gdb, StopEvent, SymbolMapping, ThreadInfo};
pub use qmp::Qmp;
pub use serial::Serial;
pub use transport::Timeout;

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use anyhow::{Context, Result, ensure};
use tempfile::TempDir;
use transport::{ChildGuard, connect_socket};

#[derive(Clone, Debug)]
pub struct VmConfig {
    pub project_root: PathBuf,
    pub firmware: PathBuf,
    pub qemu: PathBuf,
    pub gdb: PathBuf,
    pub harts: u32,
    pub memory_mib: u32,
    pub startup_timeout: Duration,
    pub command_timeout: Duration,
    /// Defaults to target/tau-debug inside the OS project; logs survive session teardown.
    pub artifacts_root: PathBuf,
}

impl VmConfig {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            firmware: "target/riscv64imac-unknown-none-elf/release/tau-qemu".into(),
            qemu: "qemu-system-riscv64".into(),
            gdb: "gdb-multiarch".into(),
            harts: 4,
            memory_mib: 4096,
            startup_timeout: Duration::from_secs(10),
            command_timeout: Duration::from_secs(5),
            artifacts_root: "target/tau-debug".into(),
        }
    }
}

/// Owns the paused VM, debugger, control sockets, serial capture and process cleanup.
/// Drop always kills and reaps both child processes; explicit shutdown also requests QMP quit.
pub struct TestVm {
    debugger: Gdb,
    qemu: ChildGuard,
    qmp: Qmp,
    serial: Serial,
    _sockets: TempDir,
    artifacts: PathBuf,
}

impl TestVm {
    pub fn spawn(config: VmConfig) -> Result<Self> {
        ensure!(
            config.harts > 0 && config.memory_mib > 0,
            "hart count and memory must be positive"
        );
        ensure!(
            !config.startup_timeout.is_zero() && !config.command_timeout.is_zero(),
            "timeouts must be positive"
        );
        let root = fs::canonicalize(&config.project_root).context("resolve OS project root")?;
        let firmware = root.join(&config.firmware);
        ensure!(
            firmware.is_file(),
            "firmware does not exist: {}",
            firmware.display()
        );
        let artifacts_root = root.join(&config.artifacts_root);
        fs::create_dir_all(&artifacts_root)?;
        let artifacts = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(artifacts_root)?
            .keep();
        Self::start(config, &root, &firmware, &artifacts)
            .with_context(|| format!("debug session failed; logs: {}", artifacts.display()))
    }

    fn start(config: VmConfig, root: &Path, firmware: &Path, artifacts: &Path) -> Result<Self> {
        // Keep Unix socket names short even when the OS checkout path is long.
        let sockets = tempfile::Builder::new()
            .prefix("tau-debug-")
            .tempdir_in("/tmp")?;
        let qmp_path = sockets.path().join("qmp");
        let gdb_path = sockets.path().join("gdb");
        let serial_path = sockets.path().join("serial");
        let mut qemu = ChildGuard(
            Command::new(&config.qemu)
                .current_dir(root)
                .args(["-M", "virt", "-accel", "tcg", "-smp"])
                .arg(config.harts.to_string())
                .arg("-m")
                .arg(config.memory_mib.to_string())
                .arg("-bios")
                .arg(firmware)
                .args(["-display", "none", "-monitor", "none", "-S"])
                .arg("-qmp")
                .arg(format!("unix:{},server=on,wait=off", qmp_path.display()))
                .arg("-chardev")
                .arg(format!(
                    "socket,id=debug,path={},server=on,wait=off",
                    gdb_path.display()
                ))
                .args(["-gdb", "chardev:debug"])
                .arg("-chardev")
                .arg(format!(
                    "socket,id=uart,path={},server=on,wait=off",
                    serial_path.display()
                ))
                .args(["-serial", "chardev:uart"])
                .stdin(Stdio::null())
                .stdout(File::create(artifacts.join("qemu-stdout.log"))?)
                .stderr(File::create(artifacts.join("qemu-stderr.log"))?)
                .spawn()
                .with_context(|| format!("start {}", config.qemu.display()))?,
        );
        let deadline = Instant::now() + config.startup_timeout;
        let qmp = Qmp::connect(
            connect_socket(&qmp_path, &mut qemu, deadline)?,
            File::create(artifacts.join("qmp.log"))?,
            config.command_timeout,
        )?;
        let serial = Serial::connect(
            connect_socket(&serial_path, &mut qemu, deadline)?,
            File::create(artifacts.join("serial.log"))?,
            config.command_timeout,
        )?;
        // QMP is available only after QEMU has initialized its command-line chardevs.
        let debugger = Gdb::connect(
            &config.gdb,
            &gdb_path,
            root,
            artifacts,
            config.command_timeout,
        )?;
        Ok(Self {
            debugger,
            qemu,
            qmp,
            serial,
            _sockets: sockets,
            artifacts: artifacts.into(),
        })
    }

    pub fn debugger(&mut self) -> &mut Gdb {
        &mut self.debugger
    }
    pub fn qmp(&mut self) -> &mut Qmp {
        &mut self.qmp
    }
    pub fn serial(&mut self) -> &mut Serial {
        &mut self.serial
    }
    pub fn artifacts(&self) -> &Path {
        &self.artifacts
    }

    /// Stop the VM if necessary, then save diagnostics for every hart GDB exposes.
    pub fn save_diagnostics(&mut self) -> Result<PathBuf> {
        let mut text = String::new();
        match self.debugger.interrupt() {
            Ok(()) => match self.debugger.threads() {
                Ok(threads) => {
                    for thread in threads {
                        text.push_str(&format!(
                            "\nHart/thread {} ({:?})\n",
                            thread.id, thread.target_id
                        ));
                        match self.debugger.select_thread(&thread.id) {
                            Ok(()) => text.push_str(&self.debugger.diagnostics()),
                            Err(err) => text.push_str(&format!("{err:#}\n")),
                        }
                    }
                }
                Err(err) => text.push_str(&format!("cannot enumerate harts: {err:#}\n")),
            },
            Err(err) => text.push_str(&format!("cannot stop target: {err:#}\n")),
        }
        let path = self.artifacts.join("diagnostics.txt");
        fs::write(&path, text)?;
        Ok(path)
    }

    /// Run an assertion block, capturing diagnostics on errors and retaining all logs.
    /// Return Result from the block; panics trigger cleanup but not diagnostic collection.
    pub fn check<T>(&mut self, test: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let result = test(self).and_then(|value| {
            self.serial.check()?;
            Ok(value)
        });
        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                let diagnostics = self.save_diagnostics();
                Err(err.context(format!(
                    "test failed; logs: {}; diagnostics: {diagnostics:?}",
                    self.artifacts.display()
                )))
            }
        }
    }

    pub fn shutdown(mut self) -> Result<()> {
        let result = self.qmp.quit();
        // Reap even if quit fails or QEMU closes its socket before replying.
        let _ = self.qemu.0.kill();
        self.qemu.0.wait()?;
        self.serial.check()?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_startup_retains_logs() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("firmware"), [0u8; 4]).unwrap();
        let mut config = VmConfig::new(root.path());
        config.firmware = "firmware".into();
        config.qemu = "/bin/false".into();
        let err = TestVm::spawn(config).err().expect("startup must fail");
        assert!(format!("{err:#}").contains("QEMU exited during startup"));
        let runs: Vec<_> = fs::read_dir(root.path().join("target/tau-debug"))
            .unwrap()
            .collect();
        assert_eq!(runs.len(), 1);
        assert!(
            runs[0]
                .as_ref()
                .unwrap()
                .path()
                .join("qemu-stderr.log")
                .is_file()
        );
    }
}
