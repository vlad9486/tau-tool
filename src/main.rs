use tau_builder::common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    command: ArgsCommand,
}

#[derive(Subcommand)]
enum ArgsCommand {
    /// Boot QEMU to a supervisor checkpoint and inspect its virtual mapping.
    #[cfg(unix)]
    Debug {
        #[clap(long, default_value = ".")]
        project_root: PathBuf,
        #[clap(
            long,
            default_value = "target/riscv64imac-unknown-none-elf/release/tau-qemu"
        )]
        firmware: PathBuf,
        #[clap(
            long,
            default_value = "target/riscv64imac-unknown-none-elf/release/supervisor"
        )]
        supervisor: PathBuf,
        /// Virtual checkpoint address; defaults to the supervisor ELF entry point.
        #[clap(long, value_parser = parse_address)]
        at: Option<u64>,
        #[clap(long, default_value = "gdb")]
        gdb: PathBuf,
        #[clap(long, default_value = "qemu-system-riscv64")]
        qemu: PathBuf,
        #[clap(long, default_value = "30", value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
    Format {
        #[clap(long)]
        path: PathBuf,
    },
    Build {
        #[clap(long)]
        qemu: bool,
        #[clap(long)]
        vf2: bool,
    },
    Update {
        #[clap(long)]
        path: PathBuf,
    },
}

fn build_rust_spl() -> anyhow::Result<()> {
    let out = Command::new("cargo")
        .args(["build", "--release", "--package=tau-visionfive2-spl"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("build spl for visionfive 2"))?;
    let out = Command::new("rust-objcopy")
        .args(["-O", "binary"])
        .arg("target/riscv64imac-unknown-none-elf/release/tau-visionfive2-spl")
        .arg("target/riscv64imac-unknown-none-elf/release/tau-visionfive2-spl.bin")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("strip spl for visionfive 2"))?;

    Ok(())
}

fn calc_spl_header(
    spl: &[u8],
    backup_offset: Option<u32>,
    version: Option<u32>,
) -> anyhow::Result<[u8; 0x400]> {
    if spl.len() > 180048 {
        return Err(anyhow::anyhow!("spl too big"));
    }
    let c = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let checksum = c.checksum(spl);

    let mut header = [0; 0x400];
    let mut write_at = |i: usize, x: u32| {
        header[(i * 4)..((i + 1) * 4)].clone_from_slice(&x.to_le_bytes());
    };
    write_at(0x00, 0x240);
    write_at(0x01, backup_offset.unwrap_or(0x200000));
    write_at(0xa1, version.unwrap_or(0x01010101));
    write_at(0xa2, spl.len() as u32);
    write_at(0xa3, 0x400);
    write_at(0xa4, checksum);

    Ok(header)
}

pub fn build_firmware_vf2() -> anyhow::Result<()> {
    let image = common::compose_tau_image()?;
    fs::write("target/tau", image)?;
    let out = Command::new("cargo")
        .env("TAU_SBI_PAYLOAD", "target/tau")
        .args([
            "build",
            "--release",
            "--package=tau-firmware",
            "--bin=tau-visionfive2",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("build rustsbi for visionfive 2"))?;
    let out = Command::new("rust-objcopy")
        .args(["-O", "binary"])
        .arg("target/riscv64imac-unknown-none-elf/release/tau-visionfive2")
        .arg("target/riscv64imac-unknown-none-elf/release/tau-visionfive2.bin")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("strip rustsbi for visionfive 2"))?;

    Ok(())
}

pub fn build_firmware_qemu() -> anyhow::Result<()> {
    let image = common::compose_tau_image()?;
    fs::write("target/tau", image)?;
    let out = Command::new("cargo")
        .env("TAU_SBI_PAYLOAD", "target/tau")
        .args([
            "build",
            "--release",
            "--package=tau-firmware",
            "--bin=tau-qemu",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("build rustsbi for qemu"))?;

    Ok(())
}

fn format<P>(path: P) -> anyhow::Result<()>
where
    P: AsRef<Path>,
{
    use std::io::{Write, SeekFrom, Seek};

    sudo::escalate_if_needed().map_err(|err| anyhow::anyhow!("sudo: {err}"))?;

    let mut disk = gpt::GptConfig::default()
        .writable(true)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .create(path)?;

    let name = "starfive_visionfive_2_u-boot-spl";
    let ty = gpt::partition_types::Type {
        guid: uuid::Uuid::parse_str("2E54B353-1271-4842-806F-E436D6AF6985").expect("this is valid"),
        os: gpt::partition_types::OperatingSystem::None,
    };
    disk.add_partition_at(name, 1, 4096, 4096, ty, 0)?;

    let name = "starfive_visionfive_2_tau";
    let ty = gpt::partition_types::Type {
        guid: uuid::Uuid::parse_str("5B193300-FC78-40CD-8002-E86C45580B47").expect("this is valid"),
        os: gpt::partition_types::OperatingSystem::None,
    };
    disk.add_partition_at(name, 2, 8192, 8192, ty, 0)?;

    let mut file = disk.write()?;
    let lb_size = 0xFF_FF_FF_FF;
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(lb_size);
    mbr.overwrite_lba0(&mut file).unwrap();

    let spl = fs::read("target/riscv64imac-unknown-none-elf/release/tau-visionfive2-spl.bin")?;
    let spl_header = calc_spl_header(&spl, None, None)?;
    let open_sbi = fs::read("target/riscv64imac-unknown-none-elf/release/tau-visionfive2.bin")?;

    file.seek(SeekFrom::Start(0x200000))?;
    file.write_all(&spl_header)?;
    file.write_all(&spl)?;
    file.seek(SeekFrom::Start(0x400000))?;
    file.write_all(&open_sbi)?;
    file.sync_all()?;

    Ok(())
}

fn update<P>(path: P) -> anyhow::Result<()>
where
    P: AsRef<Path>,
{
    use std::io::{Write, SeekFrom, Seek};

    sudo::escalate_if_needed().map_err(|err| anyhow::anyhow!("sudo: {err}"))?;

    let image = common::compose_tau_image()?;
    let mut file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(0x200000))?;
    file.write_all(&image)?;
    file.sync_all()?;

    Ok(())
}

#[cfg(unix)]
fn parse_address(value: &str) -> Result<u64, String> {
    let value = value.replace('_', "");
    if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|err| err.to_string())
    } else {
        value.parse::<u64>().map_err(|err| err.to_string())
    }
}

fn main() -> std::process::ExitCode {
    let Args { command } = Args::parse();
    let res = match command {
        #[cfg(unix)]
        ArgsCommand::Debug {
            project_root,
            firmware,
            supervisor,
            at,
            gdb,
            qemu,
            timeout,
        } => (|| -> anyhow::Result<()> {
            use std::time::Duration;
            use tau_builder::debugger::{ElfImage, SymbolMapping, TestVm, VmConfig};
            use tau_builder::debugger::riscv::PagingMode;

            let image = ElfImage::read(project_root.join(supervisor))?;
            let mut config = VmConfig::new(project_root);
            config.firmware = firmware;
            config.gdb = gdb;
            config.qemu = qemu;
            let mut vm = TestVm::spawn(config)?;
            println!("Debug logs: {}", vm.artifacts().display());
            vm.check(|vm| {
                let dbg = vm.debugger();
                dbg.add_symbols(&image, SymbolMapping::Linked)?;
                let address = at.unwrap_or(image.entry);
                let breakpoint = dbg.hardware_breakpoint(address)?;
                let stop = dbg.run_to_breakpoint(&breakpoint, Duration::from_secs(timeout))?;
                dbg.remove_breakpoint(&breakpoint)?;
                let satp = dbg.read_register("satp")?;
                let mapping = dbg.translate(satp, address)?;
                anyhow::ensure!(
                    mapping.mode != PagingMode::Bare,
                    "paging is disabled at supervisor checkpoint"
                );
                anyhow::ensure!(
                    mapping.permissions.is_some_and(|p| p.execute),
                    "supervisor checkpoint is not executable"
                );
                println!("Reached {address:#018x} on GDB thread {:?}", stop.thread_id);
                println!(
                    "satp={satp:#018x}; {:?} -> physical {:#x} (page size {:#x})",
                    mapping.mode, mapping.physical_address, mapping.page_size
                );
                println!("stvec={:#018x}", dbg.read_register("stvec")?);
                Ok(())
            })?;
            vm.shutdown()
        })(),
        ArgsCommand::Format { path } => format(path),
        ArgsCommand::Build { qemu, vf2 } => common::build_tau()
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                if qemu {
                    build_firmware_qemu()?;
                }
                if vf2 {
                    build_rust_spl()?;
                    build_firmware_vf2()?;
                }

                Ok(())
            }),
        ArgsCommand::Update { path } => update(path),
    };
    if let Err(err) = res {
        eprintln!("{err:#}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
