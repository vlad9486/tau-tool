use tau_tool::{common, build, testing};

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use clap::{Parser, Subcommand};

const SPL_MAX_SIZE: usize = 180048;
const VF2_MAX_SIZE: usize = 0x400000;

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    command: ArgsCommand,
}

#[derive(Subcommand)]
enum ArgsCommand {
    /// Build system-test and run OS integration scenarios in fresh QEMU instances.
    #[cfg(unix)]
    Test(testing::TestArgs),
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

fn build_spl() -> anyhow::Result<()> {
    let out = Command::new("cargo")
        .args(["build", "--release", "--package=tau-visionfive2-spl"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    common::bail(&out, || anyhow::anyhow!("build spl for visionfive 2"))?;

    Ok(())
}

fn calc_spl_header(
    spl: &[u8],
    backup_offset: Option<u32>,
    version: Option<u32>,
) -> anyhow::Result<[u8; 0x400]> {
    if spl.len() > SPL_MAX_SIZE {
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

fn format<P>(path: P) -> anyhow::Result<()>
where
    P: AsRef<Path>,
{
    use std::io::{Write, SeekFrom, Seek};

    // Prepare and validate both images before modifying the disk. Build outputs
    // stay ELF; flat bytes exist only in memory when writing the SD card.
    let binaries = build::Build::main(".").binaries();
    let spl = common::elf_to_raw(
        &fs::read(binaries.join("tau-visionfive2-spl"))?,
        SPL_MAX_SIZE,
    )?;
    let spl_header = calc_spl_header(&spl, None, None)?;
    let firmware = common::elf_to_raw(&fs::read(binaries.join("tau-visionfive2"))?, VF2_MAX_SIZE)?;

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

    file.seek(SeekFrom::Start(0x200000))?;
    file.write_all(&spl_header)?;
    file.write_all(&spl)?;
    file.seek(SeekFrom::Start(0x400000))?;
    file.write_all(&firmware)?;
    file.sync_all()?;

    Ok(())
}

fn update<P>(path: P) -> anyhow::Result<()>
where
    P: AsRef<Path>,
{
    use std::io::{Write, SeekFrom, Seek};

    sudo::escalate_if_needed().map_err(|err| anyhow::anyhow!("sudo: {err}"))?;

    let image = build::Build::main(".").build_tau()?;
    let mut file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(0x200000))?;
    file.write_all(&image)?;
    file.sync_all()?;

    Ok(())
}

fn main() -> std::process::ExitCode {
    let Args { command } = Args::parse();
    let builder = build::Build::main(".");
    let res = match command {
        #[cfg(unix)]
        ArgsCommand::Test(args) => testing::run(args),
        ArgsCommand::Format { path } => format(path),
        ArgsCommand::Build { qemu, vf2 } => builder
            .build_payload()
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                if qemu {
                    builder.build_qemu()?;
                }
                if vf2 {
                    build_spl()?;
                    builder.build_vf2()?;
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
