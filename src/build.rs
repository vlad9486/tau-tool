//! RISC-V build orchestration. Commands always run from the OS project root.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use anyhow::{Context, Result};
use crate::common::{self, BuildError, ComposeError};

pub const TARGET: &str = "riscv64imac-unknown-none-elf";

#[derive(Clone, Copy, Debug)]
pub enum Payload {
    System,
    Test,
}

impl Payload {
    pub fn binary(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Test => "system-test",
        }
    }
}

pub struct Build {
    pub project_root: PathBuf,
    pub payload: Payload,
}

impl Build {
    pub fn main(root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: root.into(),
            payload: Payload::System,
        }
    }

    pub fn tests(root: &Path) -> Self {
        Self {
            project_root: root.into(),
            payload: Payload::Test,
        }
    }

    pub fn binaries(&self) -> PathBuf {
        self.project_root
            .join("target")
            .join(TARGET)
            .join("release")
    }

    fn cargo(&self) -> Command {
        let mut command = Command::new("cargo");
        command
            .current_dir(&self.project_root)
            .args(["build", "--release", "--target", TARGET])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        command
    }

    pub fn build_payload(&self) -> Result<(), BuildError> {
        for binary in ["loader", "supervisor", self.payload.binary()] {
            let mut command = self.cargo();
            if binary == "loader" || binary == "supervisor" {
                command.args(["--package=supervisor", "--features=panic-never"]);
            } else {
                command.arg(format!("--package={binary}"));
            }
            if binary == "loader" {
                command.env("RUSTFLAGS", "-C relocation-model=pie");
            }
            command.arg(format!("--bin={binary}"));
            common::bail(&command.output()?, || BuildError::Cargo)?;
        }
        Ok(())
    }

    pub fn build_tau(&self) -> Result<Vec<u8>, ComposeError> {
        common::compose_tau_image_from(&self.binaries(), self.payload.binary())
    }

    /// Compose already-built binaries and embed them into QEMU firmware.
    pub fn build_qemu(&self) -> Result<PathBuf> {
        let image = self.build_tau()?;
        let payload_path = self.binaries().join("tau");
        fs::write(&payload_path, image)?;
        let mut command = self.cargo();
        command
            .env("TAU_SBI_PAYLOAD", fs::canonicalize(payload_path)?)
            .args(["--package=tau-firmware", "--bin=tau-qemu"]);
        common::bail(&command.output()?, || {
            anyhow::anyhow!("build QEMU firmware")
        })?;
        let firmware = self.binaries().join("tau-qemu");
        firmware.canonicalize().context("resolve built firmware")
    }

    /// Compose already-built binaries and return the VisionFive 2 firmware ELF.
    pub fn build_vf2(&self) -> Result<PathBuf> {
        let image = self.build_tau()?;
        let payload_path = self.binaries().join("tau");
        fs::write(&payload_path, image)?;
        let mut command = self.cargo();
        command
            .env("TAU_SBI_PAYLOAD", fs::canonicalize(payload_path)?)
            .args(["--package=tau-firmware", "--bin=tau-visionfive2"]);
        common::bail(&command.output()?, || {
            anyhow::anyhow!("build rustsbi for visionfive 2")
        })?;
        let firmware = self.binaries().join("tau-visionfive2");
        firmware.canonicalize().context("resolve built firmware")
    }
}
