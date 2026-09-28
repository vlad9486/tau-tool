# tau-tool

Build, test, and debug support for the Tau operating system. The `tau-tool`
package provides the `tau` command and the `tau_tool` Rust library. Run the
installed command from the OS repository; build artifacts belong to that repository.

Install from this checkout:

```sh
cargo install --path .
```

## VisionFive 2 images

`tau build --vf2` produces the SPL and firmware ELF files
(`tau-visionfive2-spl` and `tau-visionfive2`).

`tau format --path=/dev/sdX` reads these ELF files, converts their
file-backed loadable segments into raw images in memory, and computes the SPL
header before writing the SD card. Conversion uses load addresses, zero-fills
gaps, omits trailing BSS, and checks image size limits. The command reformats the
selected device.

## Test the OS

From the OS repository, run:

```sh
tau test
tau test timer
tau test --list
tau test --project-root /path/to/tau --timeout 30
```

The optional filter matches substrings of scenario names. An unmatched filter is
an error. `--list` needs neither a build nor QEMU/GDB. Running tests requires
`qemu-system-riscv64`, `gdb-multiarch` (override with `--qemu`/`--gdb`), and the OS
Rust toolchain with the `riscv64imac-unknown-none-elf` target.

The host runner and all host assertions live in this repository. The OS repository
contains only RISC-V code: `3-U/test` builds a `system-test` payload in place of
`system`, using the same manifest contract, loader, supervisor, and firmware.
These are OS integration scenarios, not a guest Rust unit-test harness.

The command builds once, then boots a **fresh VM per scenario**, with four harts
and 4 GiB of RAM. It currently covers:

| Scenario | Checks |
| --- | --- |
| `boot` | U-mode execution, user executable code mapping, boot arguments, valid DTB |
| `mapping` | Map two pages through UBI; verify contents, distinct physical pages, user read/write and non-executable permissions |
| `timer` | UBI wait returns a timeout after the requested guest-time deadline |
| `fault` | Deliberate unmapped load traps from U-mode with the expected cause, address, and faulting instruction |

The timeout bounds each case's execution including boot; startup and individual
debugger commands have their own timeouts. A guest failure, unexpected stop,
timeout, or VM startup error fails that case; the runner continues with remaining
cases and exits nonzero if any failed. Build errors stop the suite immediately.

Both payloads use the default `target/riscv64imac-unknown-none-elf/release`
directory, where `system` and `system-test` have distinct binary names. Image
composition writes `target/tau` and rebuilds `tau-qemu` with the selected payload;
run `tau build --qemu` to switch back to the normal system image.
A file lock serializes test commands; ordinary builds should run separately
from tests because they share firmware and symbol paths. Logs go under
`target/tau-test/suite-*/<scenario>/run-*`, including best-effort diagnostics on
execution failures. Each suite also retains `results.json`, updated after every
case. The command prints the artifact path.

### Payload protocol and adding scenarios

Protocol v1 is intentionally small and independent of Rust's type layout. The
payload exports `TAU_TEST_CONTROL`, three little-endian `u64` words: magic
`0x5441555445535431`, version `1`, and a scenario ID initially `0`. At
`tau_test_ready`, the host validates the header and writes the chosen scenario ID
through the active hart's virtual address space. The payload reads it with a
volatile load after resuming. A version or magic mismatch fails the test.

Exported naked C functions give hardware breakpoints exact instruction addresses
and stable argument registers, including under release LTO:

| Symbol | Registers at entry |
| --- | --- |
| `tau_test_ready` | `a0`: boot hart ID |
| `tau_test_progress` | `a0`: scenario ID; `a1`, `a2`: scenario-specific values |
| `tau_test_passed` | `a0`: scenario ID; `a1`: result value |
| `tau_test_failed` | `a0`: scenario ID; `a1`: failure code; `a2`: detail |

Readiness/progress return when resumed; passed/failed are terminal loops. Panics
report failure code `1` and the source line. Other codes are documented in
`3-U/test/README.md` in the OS checkout. Reports do
not depend on a syscall or UART output. The host owns VM shutdown; guest `exit`
is not a success signal. For `fault`, validated trap entry is the success signal.

To add a scenario, add a unique ID and payload branch in `3-U/test/src/main.rs`,
then register its name and host assertions in `src/testing.rs`. Keep IDs and
register meanings synchronized; bump the protocol version for incompatible
changes. The payload ELF entry is its manifest, so breakpoints use named code
symbols rather than the ELF entry address. Existing `system` drivers are not
automatically tested by replacing that payload.

## Rust API

The `tau_tool::debugger` library manages QEMU and GDB child processes and talks
to QMP, GDB/MI, and the serial socket. No shell, Python, C++ bindings, or guest agent
is involved. OS test scenarios live in this host repository. To use the library
from another host project, add:

```toml
[dependencies]
tau-tool = { path = "/path/to/tau-tool" }
anyhow = "1"
```

Keep host projects separate from the RISC-V OS workspace.

```rust,no_run
use std::{path::Path, time::Duration};
use tau_tool::debugger::{ElfImage, SymbolMapping, TestVm, VmConfig};

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

After `tau test` has built an OS checkout, also exercise the runner's
guest-failure and execution-timeout diagnostics against that payload:

```sh
TAU_TEST_PROJECT=/path/to/tau cargo test --lib payload_failure_and_timeout -- --ignored
```

The normal suite tests MI parsing, asynchronous stops, timeout recovery, physical
memory mode restoration, QMP negotiation, and page-table walking. The ignored
integration tests need real QEMU/GDB. They generate tiny firmware fixtures without
requiring a cross compiler or the OS repository, and exercise relocated symbols,
high virtual addresses, an anonymous supervisor software-interrupt handler,
trap return, serial input/output, and process shutdown. They do not replace tests
of the OS's SBI timer and PLIC driver.
