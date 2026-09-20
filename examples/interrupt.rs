//! Host-side interrupt test. Supply checkpoints from the OS being tested.

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use std::{path::PathBuf, time::Duration};
    use clap::{Parser, ValueEnum};
    use tau_builder::debugger::{ElfImage, SymbolMapping, TestVm, VmConfig, riscv};

    #[derive(Clone, Copy, ValueEnum)]
    enum Source {
        Timer,
        Uart,
    }

    #[derive(Parser)]
    struct Args {
        #[arg(long, default_value = ".")]
        project_root: PathBuf,
        /// Virtual address reached after stvec and the interrupt source are configured.
        #[arg(long, value_parser = address)]
        ready: u64,
        /// Virtual address proving system progress after the handler returns.
        #[arg(long, value_parser = address)]
        progress: u64,
        #[arg(long, value_enum)]
        source: Source,
        #[arg(long, default_value = "90")]
        uart_byte: u8,
        #[arg(long, default_value = "gdb-multiarch")]
        gdb: PathBuf,
        #[arg(long, default_value = "30", value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    }

    fn address(value: &str) -> Result<u64, String> {
        let value = value.replace('_', "");
        u64::from_str_radix(value.strip_prefix("0x").unwrap_or(&value), 16)
            .map_err(|err| err.to_string())
    }

    let args = Args::parse();
    let image = ElfImage::read(
        args.project_root
            .join("target/riscv64imac-unknown-none-elf/release/supervisor"),
    )?;
    let mut config = VmConfig::new(&args.project_root);
    config.gdb = args.gdb;
    let mut vm = TestVm::spawn(config)?;
    println!("Logs: {}", vm.artifacts().display());
    vm.check(|vm| {
        let debugger = vm.debugger();
        debugger.add_symbols(&image, SymbolMapping::Linked)?;
        let ready = debugger.hardware_breakpoint(args.ready)?;
        let timeout = Duration::from_secs(args.timeout);
        debugger.run_to_breakpoint(&ready, timeout)?;
        debugger.remove_breakpoint(&ready)?;

        let cause = match args.source {
            Source::Timer => riscv::SUPERVISOR_TIMER,
            Source::Uart => riscv::SUPERVISOR_EXTERNAL,
        };
        let handler = debugger.trap_breakpoint(cause)?;
        if matches!(args.source, Source::Uart) {
            vm.serial().send(&[args.uart_byte])?;
        }
        let trap = vm
            .debugger()
            .run_until_interrupt(&handler, cause, timeout)?;
        println!("Interrupt {} interrupted PC {:#x}", trap.cause(), trap.sepc);
        vm.debugger().remove_breakpoint(&handler)?;

        let progress = vm.debugger().hardware_breakpoint(args.progress)?;
        vm.debugger().run_to_breakpoint(&progress, timeout)?;
        // For UART tests, also assert the guest's claimed PLIC ID and received byte
        // here using read_register/read_virtual at suitable OS-specific checkpoints.
        // Never read the PLIC claim register from the debugger: the read has side effects.
        Ok(())
    })?;
    vm.shutdown()
}

#[cfg(not(unix))]
fn main() {
    eprintln!("The debugger currently requires Unix sockets.");
    std::process::exit(1);
}
