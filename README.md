# tau-builder

Build tooling and a host-side QEMU/GDB debugger for Tau. Run the installed tool
from the OS repository; build artifacts belong to that repository.

## Debug a supervisor checkpoint

The debugger needs a Unix host, `qemu-system-riscv64`, and a RISC-V-capable GDB
with MI3 support (for example `gdb-multiarch`). Install the tool from this checkout:

```sh
cargo install --path .
```

Then, from the OS repository after building its firmware:

```sh
tau-builder debug
```

This starts the existing OpenSBI payload in QEMU `virt`, with TCG, four harts,
and 4 GiB of RAM. QEMU starts paused. The tool loads supervisor symbols on the
host, installs a hardware breakpoint at the supervisor ELF entry, and continues.
On reaching that checkpoint it selects the stopping hart, walks the page tables,
checks that paging is enabled and the checkpoint is executable, prints the mapping
and `stvec`, and shuts down. It does not require the other harts to run OS code.
This is a finite boot check, not an interactive debugger terminal.

Override the checkpoint or executable paths when needed:

```sh
tau-builder debug --at 0xffffffc000001000 --timeout 30 --gdb riscv64-linux-gnu-gdb
tau-builder debug --project-root /path/to/tau
```

`--at` accepts a decimal or `0x` hexadecimal virtual address, including underscores.
It should be a supervisor checkpoint after the address-space switch. The default
firmware and supervisor paths are:

```text
target/opensbi-qemu/build/platform/generic/firmware/fw_payload.elf
target/riscv64imac-unknown-none-elf/release/supervisor
```

Use `--firmware` and `--supervisor` to override them. Relative paths resolve from
`--project-root`, which defaults to the caller's working directory. The debugger
does not rebuild the OS or create a ramdisk. Boot/test failures return a nonzero
exit status.

## Rust API

The `tau_builder::debugger` library manages QEMU and GDB child processes and talks
to QMP, GDB/MI, and the serial socket. No shell, Python, C++ bindings, or guest agent
is involved. Add it as a dependency of a **host-target test crate** in the OS repo:

```toml
[dependencies]
tau-builder = { path = "/path/to/tau-builder" }
anyhow = "1"
```

Build that test crate for the host, not `riscv64imac-unknown-none-elf`; if the OS's
Cargo configuration defaults to RISC-V, pass `--target` with the host triple.

```rust,no_run
use std::{path::Path, time::Duration};
use tau_builder::debugger::{ElfImage, SymbolMapping, TestVm, VmConfig};

fn check_boot(project: &Path) -> anyhow::Result<()> {
    let supervisor = ElfImage::read(project.join(
        "target/riscv64imac-unknown-none-elf/release/supervisor",
    ))?;
    let mut vm = TestVm::spawn(VmConfig::new(project))?;
    vm.check(|vm| {
        let dbg = vm.debugger();
        dbg.add_symbols(&supervisor, SymbolMapping::Linked)?;
        let entry = dbg.hardware_breakpoint(supervisor.entry)?;
        let stop = dbg.run_to_breakpoint(&entry, Duration::from_secs(30))?;
        dbg.remove_breakpoint(&entry)?;

        let satp = dbg.read_register("satp")?;
        let mapping = dbg.translate(satp, supervisor.entry)?;
        anyhow::ensure!(mapping.permissions.is_some_and(|p| p.execute));
        println!("{stop:?}: {mapping:?}");
        Ok(())
    })?;
    vm.shutdown()
}
```

`VmConfig` also exposes the firmware path, executable paths, hart count, memory
size, command timeout, socket-startup timeout, and artifact directory. Execution
waits have their own timeout; individual protocol commands and timeout recovery
use `command_timeout`.

Symbol mappings are independent for each ELF:

* `SymbolMapping::Linked`: execute at the ELF addresses, including high virtual
  addresses for the supervisor. Use this for `system` if it also executes at its
  linked addresses.
* `SymbolMapping::LoadAt(0x8020_0000)`: place the lowest nonempty load segment at
  this address, matching the flat loader's placement by the image composer.
* `SymbolMapping::Offset(bias)`: explicitly add a load bias to all symbol addresses.

Symbol loading never uploads or relocates guest memory. Keep the matching ELF
files on the host, even when the boot payload contains only flat loader/supervisor
bytes. Embedding the system ELF with debug sections is also supported by the
existing build flow.

## Interrupt tests

After the OS configures `stvec`, call `trap_breakpoint(cause)` to discover its
actual entry address. This supports Direct and Vectored modes and does not need
a symbol for the naked trap handler. Refresh the breakpoint if the OS changes
`stvec` or the relevant address space. GDB automatically selects the hart reported
by a stop event; no boot hart ID is assumed.

Use `run_until_interrupt` with `riscv::SUPERVISOR_TIMER` (5) or
`riscv::SUPERVISOR_EXTERNAL` (9). It reads `scause`, `sepc`, `stval`, `sstatus`, and
`satp` at trap entry, before the handler modifies saved state. It continues past
other interrupts and U-mode ecalls, and fails on other synchronous traps or
unrelated breakpoints. Interrupt tests use continue, not instruction stepping.
Remove an earlier checkpoint breakpoint before continuing if it can be hit again.

The executable [interrupt example](examples/interrupt.rs) accepts OS-specific
readiness and progress addresses:

```sh
cargo run --example interrupt -- \
  --project-root /path/to/tau \
  --ready 0xffffffc000001000 \
  --progress 0xffffffc000002000 \
  --source timer
```

Those addresses are placeholders. For a timer, the readiness checkpoint must be
after the guest schedules its SBI timer. For UART, use `--source uart --uart-byte 90`
and a checkpoint after the guest enables the UART source and its PLIC context.
The example observes delivery and subsequent system progress. To assert UART
identity and data, extend it at suitable checkpoints to inspect the guest's saved
PLIC claim ID and received byte. Cause 9 alone does not identify the UART. Do not
read the PLIC claim register from the debugger: that would claim the interrupt.

`read_virtual` uses the selected hart's current translation context;
`read_physical` temporarily selects QEMU's physical-memory mode and restores the
previous mode even on a read error. `translate` walks base Sv39/Sv48/Sv57 tables,
including superpages, and checks canonical addresses and PTE validity. It reports
permissions but does not simulate privilege/PMP/SUM/MXR checks or A/D updates.
Extension-specific PTE encodings such as Svnapot and Svpbmt are rejected explicitly.
Memory reads are limited to 1 MiB per call.

## Failure handling and logs

Each run keeps logs under the OS project's `target/tau-debug/run-*`:

* `serial.log`: raw UART output; a 64 KiB tail is also available through the API.
* `gdb-mi.log`, `gdb-requests.log`, `gdb-stderr.log`: debugger protocol and errors.
* `qmp.log`, `qemu-stdout.log`, `qemu-stderr.log`: VM control responses and errors.
* `diagnostics.txt`: best-effort registers and stack traces for every exposed hart
  when a `vm.check` closure returns an error.

Return assertion failures as `Result` from `vm.check` (for example with
`anyhow::ensure!`) to collect diagnostics. Panics still trigger process cleanup,
but cannot collect those diagnostics. Use `save_diagnostics` for a manual snapshot.
Dropping `TestVm` terminates and reaps both children, including partial startup
failures. Socket directories are private and removed; logs are retained.

`wait_for_stop` does not resume the target and leaves it running on timeout.
`run_to_breakpoint` attempts to interrupt on timeout so registers remain available.
Unexpected stops are returned as errors rather than silently continued. A command
reply timeout makes that protocol connection unusable; start a new session.
Use GDB for execution control while attached; issuing QMP `cont`, `stop`, or reset
commands directly can invalidate the debugger's tracked state.

## Validation

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --test qemu_debugger -- --ignored
```

The normal suite tests MI parsing, asynchronous stops, timeout recovery, physical
memory mode restoration, QMP negotiation, and page-table walking. The ignored
integration tests need real QEMU/GDB. They generate tiny firmware fixtures without
requiring a cross compiler or the OS repository, and exercise relocated symbols,
high virtual addresses, an anonymous supervisor software-interrupt handler,
trap return, serial input/output, and process shutdown. They do not replace tests
of the OS's SBI timer and PLIC driver.
