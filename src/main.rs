pub mod common;

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
    BuildFirmware,
    Format {
        #[clap(long)]
        path: PathBuf,
    },
    BuildTau {
        #[clap(long)]
        qemu: bool,
    },
    Update {
        #[clap(long)]
        path: PathBuf,
    },
}

fn build_spl() -> anyhow::Result<()> {
    const REVISION: &str = "c4c67bb66ae6f41c98537d18cf5c3abc8b97b8e4";
    const REPO: &str = "https://github.com/starfive-tech/u-boot.git";
    let dir = common::git_clone("target", REPO, REVISION, "u-boot-vf2")?;

    let out_file = <str as AsRef<Path>>::as_ref("target/u-boot-vf2-build/spl/u-boot-spl.bin");
    if out_file.exists() {
        return Ok(());
    }

    Command::new("git")
        .current_dir(&dir)
        .args(["checkout", "."])
        .output()?;
    let out = Command::new("git")
        .current_dir(&dir)
        .args([
            "apply",
            "../../board/jh7110-starfive-visionfive-2-v1.3b-u-boot.patch",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("apply u-boot patch"))?;

    fs::create_dir("target/u-boot-vf2-build").unwrap_or_default();

    let args = &[
        "O=../u-boot-vf2-build",
        "CROSS_COMPILE=riscv64-linux-gnu-",
        "ARCH=riscv",
    ];
    let invocations = [
        args.iter().copied().chain(Some("olddefconfig")),
        args.iter()
            .copied()
            .chain(Some("starfive_visionfive2_defconfig")),
        args.iter().copied().chain(None),
    ];
    for invocation in invocations {
        let out = Command::new("make")
            .current_dir(&dir)
            .args(invocation)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .output()?;
        common::bail(&out, || anyhow::anyhow!("build u-boot"))?;
    }

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

fn build_opensbi() -> anyhow::Result<()> {
    const REVISION: &str = "1725bd71080960290fdde4499a58c25c09d5c8ee";
    const REPO: &str = "https://github.com/starfive-tech/opensbi.git";
    let dir = common::git_clone("target", REPO, REVISION, "opensbi-vf2")?;

    let out = Command::new("make")
        .current_dir(dir)
        .args([
            "CC=clang",
            "LD=ld.lld",
            "LLVM=1",
            "PLATFORM=generic",
            "FW_FDT_PATH=../../board/jh7110-starfive-visionfive-2-v1.3b.dtb",
            // "FW_PAYLOAD_PATH=../tau",
            "FW_TEXT_START=0x40000000",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("build opensbi for vf2"))?;

    // "target/opensbi-vf2/build/platform/generic/firmware/fw_payload.bin"
    Ok(())
}

fn build_opensbi_qemu() -> anyhow::Result<()> {
    const REVISION: &str = "74434f255873d74e56cc50aa762d1caf24c099f8";
    const REPO: &str = "https://github.com/riscv-software-src/opensbi.git";
    let dir = common::git_clone("target", REPO, REVISION, "opensbi-qemu")?;
    let image = common::compose_tau_image()?;
    fs::write("target/tau", image)?;

    let out = Command::new("make")
        .current_dir(dir)
        .args([
            "CC=clang",
            "LD=ld.lld",
            "LLVM=1",
            "PLATFORM=generic",
            "FW_FDT_PATH=../../board/qemu-riscv-virt.dtb",
            "FW_PAYLOAD_PATH=../tau",
            "FW_TEXT_START=0x80000000",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("build opensbi for qemu"))?;
    // "target/opensbi-qemu/build/platform/generic/firmware/fw_payload.elf"

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

    let name = "starfive_visionfive_2_u-boot";
    let ty = gpt::partition_types::Type {
        guid: uuid::Uuid::parse_str("5B193300-FC78-40CD-8002-E86C45580B47").expect("this is valid"),
        os: gpt::partition_types::OperatingSystem::None,
    };
    disk.add_partition_at(name, 2, 8192, 8192, ty, 0)?;

    let mut file = disk.write()?;
    let lb_size = 0xFF_FF_FF_FF;
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(lb_size);
    mbr.overwrite_lba0(&mut file).unwrap();

    let spl = fs::read("target/u-boot-vf2-build/spl/u-boot-spl.bin")?;
    let spl_header = calc_spl_header(&spl, None, None)?;
    let open_sbi = fs::read("target/opensbi-vf2/build/platform/generic/firmware/fw_payload.bin")?;

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

fn main() {
    let Args { command } = Args::parse();
    let res = match command {
        ArgsCommand::BuildFirmware => build_spl().and_then(|()| build_opensbi()),
        ArgsCommand::Format { path } => format(path),
        ArgsCommand::BuildTau { qemu } => {
            if qemu {
                common::build_tau()
                    .map_err(anyhow::Error::from)
                    .and_then(|()| build_opensbi_qemu())
            } else {
                common::build_tau().map_err(anyhow::Error::from)
            }
        }
        ArgsCommand::Update { path } => update(path),
    };
    if let Err(err) = res {
        eprintln!("{err}");
    }
}
