use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use object::{Object, ObjectSegment, ObjectSymbol};

use super::{
    mi::{self, Record, Value},
    riscv::{self, Translation, TrapSnapshot},
    transport::{ChildGuard, Lines, Timeout, remaining},
};

/// The original ELF stays on the host; symbol loading never uploads guest code.
#[derive(Clone, Debug)]
pub struct ElfImage {
    pub path: PathBuf,
    pub entry: u64,
    pub minimum_load_address: u64,
}

impl ElfImage {
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = fs::canonicalize(path.as_ref()).context("resolve ELF path")?;
        let bytes = fs::read(&path)?;
        let elf = object::File::parse(bytes.as_slice())?;
        ensure!(
            elf.format() == object::BinaryFormat::Elf
                && elf.architecture() == object::Architecture::Riscv64
                && elf.is_little_endian(),
            "expected a little-endian RISC-V 64 ELF"
        );
        let minimum_load_address = elf
            .segments()
            .filter(|s| s.size() != 0)
            .map(|s| s.address())
            .min()
            .context("ELF has no loadable segments")?;
        Ok(Self {
            path,
            entry: elf.entry(),
            minimum_load_address,
        })
    }

    pub fn runtime_entry(&self, mapping: SymbolMapping) -> u64 {
        self.entry.wrapping_add(mapping.offset(self))
    }

    /// Resolve an exact ELF symbol, without GDB language parsing or prologue skipping.
    pub fn symbol_address(&self, name: &str, mapping: SymbolMapping) -> Result<u64> {
        let bytes = fs::read(&self.path)?;
        let elf = object::File::parse(bytes.as_slice())?;
        let symbol = elf
            .symbols()
            .find(|s| s.is_definition() && s.name().ok() == Some(name))
            .with_context(|| format!("missing symbol {name} in {}", self.path.display()))?;
        Ok(symbol.address().wrapping_add(mapping.offset(self)))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SymbolMapping {
    /// ELF addresses already match the execution addresses (including high virtual addresses).
    Linked,
    /// Add this bias to ELF addresses, modulo the RV64 address space.
    Offset(u64),
    /// Place the lowest nonempty load segment at this address, as the image composer does.
    LoadAt(u64),
}

impl SymbolMapping {
    fn offset(self, image: &ElfImage) -> u64 {
        match self {
            Self::Linked => 0,
            Self::Offset(offset) => offset,
            Self::LoadAt(base) => base.wrapping_sub(image.minimum_load_address),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct StopEvent {
    pub reason: String,
    pub breakpoint: Option<String>,
    pub thread_id: Option<String>,
    pub pc: Option<u64>,
    pub signal: Option<String>,
}

impl StopEvent {
    fn from_mi(data: &Value) -> Result<Self> {
        let string = |name| {
            data.get(name)
                .and_then(|v| v.string().ok())
                .map(str::to_owned)
        };
        Ok(Self {
            reason: string("reason").unwrap_or_else(|| "stopped".into()),
            breakpoint: string("bkptno"),
            thread_id: string("thread-id"),
            pc: data
                .get("frame")
                .and_then(|v| v.get("addr"))
                .and_then(|v| v.string().ok())
                .and_then(|s| number(s).ok()),
            signal: string("signal-name"),
        })
    }

    pub fn hit(&self, breakpoint: &Breakpoint) -> bool {
        self.reason == "breakpoint-hit"
            && self.breakpoint.as_ref().is_some_and(|id| {
                id == &breakpoint.id || id.split('.').next() == Some(breakpoint.id.as_str())
            })
    }
}

#[derive(Clone, Debug)]
pub struct ThreadInfo {
    pub id: String,
    pub target_id: Option<String>,
    pub state: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Stopped,
    Running,
    Exited,
}

struct Reply {
    data: Value,
    console: String,
}

/// Synchronous Rust API over an asynchronous GDB/MI session.
/// Methods serialize commands; asynchronous stop events are retained until consumed.
pub struct Gdb {
    // Drop the process before dropping its pipes, including on partial initialization failures.
    _child: ChildGuard,
    input: ChildStdin,
    output: Lines,
    requests: File,
    timeout: Duration,
    next_token: u64,
    pending: VecDeque<StopEvent>,
    registers: HashMap<String, usize>,
    state: State,
    healthy: bool,
}

impl Gdb {
    pub(super) fn connect(
        program: &Path,
        socket: &Path,
        project: &Path,
        logs: &Path,
        timeout: Duration,
    ) -> Result<Self> {
        let mut child = ChildGuard(
            Command::new(program)
                .current_dir(project)
                .args(["--interpreter=mi3", "--nx", "--quiet"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(File::create(logs.join("gdb-stderr.log"))?)
                .spawn()
                .with_context(|| {
                    format!(
                        "start {} (a RISC-V-capable GDB is required)",
                        program.display()
                    )
                })?,
        );
        let input = child.0.stdin.take().context("GDB stdin unavailable")?;
        let output = Lines::spawn(
            child.0.stdout.take().context("GDB stdout unavailable")?,
            Some(File::create(logs.join("gdb-mi.log"))?),
        );
        let mut gdb = Self {
            _child: child,
            input,
            output,
            requests: File::create(logs.join("gdb-requests.log"))?,
            timeout,
            next_token: 1,
            pending: VecDeque::new(),
            registers: HashMap::new(),
            state: State::Stopped,
            healthy: true,
        };
        for command in [
            "-gdb-set pagination off",
            "-gdb-set confirm off",
            "-gdb-set mi-async on",
            "-gdb-set non-stop off",
        ] {
            gdb.command(command)?;
        }
        // Unlike most MI commands, target-select forwards its parameters verbatim to
        // the target backend. Quotes would become part of the Unix socket filename.
        let socket = path_text(socket)?;
        ensure!(
            !socket.chars().any(char::is_whitespace),
            "GDB socket path contains whitespace"
        );
        gdb.command(&format!("-target-select remote {socket}"))?;
        // Flush the initial attach stop before the first user-visible resume.
        gdb.command("-thread-info")?;
        gdb.pending.clear();
        gdb.state = State::Stopped;
        let names = gdb.command("-data-list-register-names")?.data;
        for (index, name) in names.field("register-names")?.list()?.iter().enumerate() {
            let name = name.string()?;
            if !name.is_empty() {
                gdb.registers.insert(name.into(), index);
            }
        }
        // GDB calls x8 fp; Rust/assembly often calls it s0. Accept both ABI and xN names.
        let abi = [
            "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "fp", "s1", "a0", "a1", "a2", "a3",
            "a4", "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11",
            "t3", "t4", "t5", "t6",
        ];
        for (register, name) in abi.into_iter().enumerate() {
            let numeric = format!("x{register}");
            if let Some(index) = gdb
                .registers
                .get(name)
                .or_else(|| gdb.registers.get(&numeric))
                .copied()
            {
                gdb.registers.insert(numeric, index);
                gdb.registers.insert(name.into(), index);
            }
        }
        if let Some(index) = gdb
            .registers
            .get("fp")
            .or_else(|| gdb.registers.get("s0"))
            .copied()
        {
            gdb.registers.insert("s0".into(), index);
            gdb.registers.insert("fp".into(), index);
        }
        ensure!(
            gdb.registers.contains_key("satp") && gdb.registers.contains_key("pc"),
            "GDB target does not expose the expected RISC-V registers"
        );
        Ok(gdb)
    }

    fn command(&mut self, command: &str) -> Result<Reply> {
        ensure!(
            self.healthy,
            "GDB command transport is unusable; create a new session"
        );
        ensure!(!command.contains(['\n', '\r']), "newline in GDB command");
        let token = self.next_token;
        self.next_token += 1;
        let result = self.exchange(token, command);
        // A lost response leaves the command outcome unknown. Do not silently reuse it.
        if result.as_ref().is_err_and(|err| !err.is::<CommandError>()) {
            self.healthy = false;
        }
        result
    }

    fn exchange(&mut self, token: u64, command: &str) -> Result<Reply> {
        let deadline = Instant::now() + self.timeout;
        writeln!(self.requests, "{token}{command}")?;
        writeln!(self.input, "{token}{command}")?;
        self.input.flush()?;
        let mut console = String::new();
        loop {
            let record = mi::parse(&self.output.recv(deadline, "GDB command reply")?)?;
            match record {
                Record::Result {
                    token: reply_token,
                    class,
                    data,
                } => {
                    ensure!(
                        reply_token == Some(token),
                        "GDB reply token mismatch: expected {token}, got {reply_token:?}"
                    );
                    if class == "error" {
                        return Err(CommandError(
                            data.get("msg")
                                .and_then(|v| v.string().ok())
                                .unwrap_or("GDB error")
                                .into(),
                        )
                        .into());
                    }
                    return Ok(Reply { data, console });
                }
                Record::Stream { kind: b'~', text } => console.push_str(&text),
                record => self.event(record)?,
            }
        }
    }

    fn event(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Async {
                kind: b'*',
                class,
                data,
            } if class == "stopped" => {
                let stop = StopEvent::from_mi(&data)?;
                self.state = if stop.reason.starts_with("exited") {
                    State::Exited
                } else {
                    State::Stopped
                };
                self.pending.push_back(stop);
            }
            Record::Async {
                kind: b'*', class, ..
            } if class == "running" => self.state = State::Running,
            Record::Result { .. } => bail!("unsolicited GDB result record"),
            _ => {}
        }
        Ok(())
    }

    fn stopped(&self) -> Result<()> {
        ensure!(
            self.state == State::Stopped,
            "operation requires a stopped target"
        );
        Ok(())
    }

    pub fn add_symbols(&mut self, image: &ElfImage, mapping: SymbolMapping) -> Result<()> {
        self.stopped()?;
        let command = format!(
            "add-symbol-file {} -o {:#x}",
            mi::quote(path_text(&image.path)?),
            mapping.offset(image)
        );
        self.command(&format!(
            "-interpreter-exec console {}",
            mi::quote(&command)
        ))?;
        Ok(())
    }

    pub fn threads(&mut self) -> Result<Vec<ThreadInfo>> {
        let reply = self.command("-thread-info")?.data;
        reply
            .field("threads")?
            .list()?
            .iter()
            .map(|v| {
                Ok(ThreadInfo {
                    id: v.field("id")?.string()?.into(),
                    target_id: v
                        .get("target-id")
                        .map(Value::string)
                        .transpose()?
                        .map(str::to_owned),
                    state: v
                        .get("state")
                        .map(Value::string)
                        .transpose()?
                        .map(str::to_owned),
                })
            })
            .collect()
    }

    pub fn select_thread(&mut self, id: &str) -> Result<()> {
        self.stopped()?;
        ensure!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit() || b == b'.'),
            "invalid GDB thread ID"
        );
        self.command(&format!("-thread-select {id}"))?;
        Ok(())
    }

    pub fn read_register(&mut self, name: &str) -> Result<u64> {
        self.stopped()?;
        let index = self
            .registers
            .get(name)
            .with_context(|| format!("unknown target register {name}"))?;
        let reply = self
            .command(&format!("-data-list-register-values x {index}"))?
            .data;
        let values = reply.field("register-values")?.list()?;
        let value = values
            .first()
            .context("GDB returned no register value")?
            .field("value")?
            .string()?;
        number(value).with_context(|| format!("read register {name}"))
    }

    pub fn hardware_breakpoint(&mut self, address: u64) -> Result<Breakpoint> {
        self.stopped()?;
        let reply = self
            .command(&format!("-break-insert -h *{address:#x}"))?
            .data;
        Ok(Breakpoint {
            id: reply.field("bkpt")?.field("number")?.string()?.into(),
        })
    }

    /// Resolve a function name using the host-side symbols and insert a hardware breakpoint.
    pub fn hardware_breakpoint_symbol(&mut self, symbol: &str) -> Result<Breakpoint> {
        self.stopped()?;
        let reply = self
            .command(&format!("-break-insert -h {}", mi::quote(symbol)))?
            .data;
        Ok(Breakpoint {
            id: reply.field("bkpt")?.field("number")?.string()?.into(),
        })
    }

    pub fn remove_breakpoint(&mut self, breakpoint: &Breakpoint) -> Result<()> {
        ensure!(
            !breakpoint.id.is_empty()
                && breakpoint
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.'),
            "invalid breakpoint ID"
        );
        self.command(&format!("-break-delete {}", breakpoint.id))?;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        self.stopped()?;
        ensure!(
            self.pending.is_empty(),
            "consume pending stops with wait_for_stop before resuming"
        );
        self.state = State::Running;
        if let Err(err) = self.command("-exec-continue --all") {
            self.state = State::Stopped;
            return Err(err);
        }
        Ok(())
    }

    /// Wait without resuming. A timeout leaves the target running; interrupt can recover it.
    /// Selects the stopping hart automatically before returning.
    pub fn wait_for_stop(&mut self, timeout: Duration) -> Result<StopEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(stop) = self.pending.pop_front() {
                if self.state == State::Stopped
                    && let Some(id) = &stop.thread_id
                {
                    self.select_thread(id)?;
                }
                return Ok(stop);
            }
            let record = mi::parse(&self.output.recv(deadline, "target stop")?)?;
            self.event(record)?;
        }
    }

    pub fn interrupt(&mut self) -> Result<()> {
        if self.state == State::Running {
            self.command("-exec-interrupt --all")?;
        }
        if self.state == State::Running || !self.pending.is_empty() {
            self.wait_for_stop(self.timeout)?;
        }
        self.stopped()
    }

    /// Resume to a particular breakpoint; unexpected stops are errors and stay stopped.
    /// On timeout, attempt to stop the target for inspection.
    pub fn run_to_breakpoint(
        &mut self,
        breakpoint: &Breakpoint,
        timeout: Duration,
    ) -> Result<StopEvent> {
        self.resume()?;
        let stop = match self.wait_for_stop(timeout) {
            Ok(stop) => stop,
            Err(err) => {
                if err.is::<Timeout>()
                    && let Err(stop_error) = self.interrupt()
                {
                    return Err(
                        err.context(format!("also failed to interrupt target: {stop_error:#}"))
                    );
                }
                return Err(err);
            }
        };
        ensure!(
            stop.hit(breakpoint),
            "unexpected stop while waiting for breakpoint {}: {stop:?}",
            breakpoint.id
        );
        Ok(stop)
    }

    /// Break at the current stvec entry for the requested interrupt cause.
    /// Must be called after the guest has configured stvec on the active hart.
    pub fn trap_breakpoint(&mut self, interrupt_cause: u64) -> Result<Breakpoint> {
        let stvec = self.read_register("stvec")?;
        self.hardware_breakpoint(riscv::trap_address(stvec, Some(interrupt_cause))?)
    }

    pub fn trap_snapshot(&mut self) -> Result<TrapSnapshot> {
        Ok(TrapSnapshot {
            scause: self.read_register("scause")?,
            sepc: self.read_register("sepc")?,
            stval: self.read_register("stval")?,
            sstatus: self.read_register("sstatus")?,
            satp: self.read_register("satp")?,
        })
    }

    /// Wait at the supplied trap breakpoint, passing U-mode ecalls and other interrupts.
    /// Other synchronous exceptions fail immediately. Uses one deadline for the whole wait.
    pub fn run_until_interrupt(
        &mut self,
        breakpoint: &Breakpoint,
        cause: u64,
        timeout: Duration,
    ) -> Result<TrapSnapshot> {
        let deadline = Instant::now() + timeout;
        loop {
            self.run_to_breakpoint(breakpoint, remaining(deadline, "requested interrupt")?)?;
            let trap = self.trap_snapshot()?;
            if trap.is_interrupt() && trap.cause() == cause {
                return Ok(trap);
            }
            ensure!(
                trap.is_interrupt() || trap.cause() == 8,
                "unexpected synchronous trap: {trap:?}"
            );
        }
    }

    fn packet(&mut self, packet: &str) -> Result<String> {
        let reply = self.command(&format!(
            "-interpreter-exec console {}",
            mi::quote(&format!("maintenance packet {packet}"))
        ))?;
        // This QEMU extension has no native MI operation. Only decode the packet envelope,
        // never human-readable register/memory output.
        for line in reply.console.lines() {
            if let Some(value) = line.trim().strip_prefix("received: ")
                && let Record::Stream { text, .. } = mi::parse(&format!("~{value}"))?
            {
                return Ok(text);
            }
        }
        bail!(
            "GDB did not report a remote packet response: {:?}",
            reply.console
        )
    }

    fn memory_mode<T>(
        &mut self,
        physical: bool,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.stopped()?;
        let old = self.packet("qqemu.PhyMemMode")?;
        ensure!(
            old == "0" || old == "1",
            "QEMU physical-memory extension unavailable: {old:?}"
        );
        let new = if physical { "1" } else { "0" };
        if old != new {
            ensure!(
                self.packet(&format!("Qqemu.PhyMemMode:{new}"))? == "OK",
                "cannot change QEMU memory mode"
            );
        }
        let result = f(self);
        if old != new {
            match self.packet(&format!("Qqemu.PhyMemMode:{old}")) {
                Ok(reply) if reply == "OK" => {}
                restore => bail!(
                    "failed to restore QEMU memory mode: {restore:?}; operation error: {:?}",
                    result.as_ref().err()
                ),
            }
        }
        result
    }

    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        ensure!(length <= 1024 * 1024, "read at most 1 MiB per request");
        if length == 0 {
            return Ok(Vec::new());
        }
        let end = address
            .checked_add(length as u64)
            .context("memory range overflows")?;
        let reply = self
            .command(&format!("-data-read-memory-bytes {address:#x} {length}"))?
            .data;
        let mut output = Vec::with_capacity(length);
        for block in reply.field("memory")?.list()? {
            let begin = number(block.field("begin")?.string()?)?;
            ensure!(
                begin == address + output.len() as u64,
                "GDB returned a partial or noncontiguous memory read"
            );
            let contents = block.field("contents")?.string()?;
            ensure!(
                contents.len() % 2 == 0 && contents.is_ascii(),
                "invalid memory hex data"
            );
            for i in (0..contents.len()).step_by(2) {
                output.push(u8::from_str_radix(&contents[i..i + 2], 16)?);
            }
            ensure!(output.len() <= length, "GDB returned too much memory");
        }
        ensure!(
            address + output.len() as u64 == end,
            "GDB returned a partial memory read"
        );
        Ok(output)
    }

    pub fn read_virtual(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.memory_mode(false, |gdb| gdb.read_memory(address, length))
    }

    /// Write through the selected hart's virtual translation context while stopped.
    /// Like reads, restores QEMU's previous physical-memory mode even on failure.
    pub fn write_virtual(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= 1024 * 1024,
            "write at most 1 MiB per request"
        );
        address
            .checked_add(bytes.len() as u64)
            .context("memory range overflows")?;
        self.memory_mode(false, |gdb| {
            if !bytes.is_empty() {
                use std::fmt::Write as _;
                let mut hex = String::with_capacity(bytes.len() * 2);
                for byte in bytes {
                    write!(hex, "{byte:02x}")?;
                }
                gdb.command(&format!("-data-write-memory-bytes {address:#x} {hex}"))?;
            }
            Ok(())
        })
    }

    pub fn read_physical(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.memory_mode(true, |gdb| gdb.read_memory(address, length))
    }

    /// Walk base Sv39/Sv48/Sv57 page tables. Returns mapping/flags, not an access check.
    pub fn translate(&mut self, satp: u64, virtual_address: u64) -> Result<Translation> {
        self.memory_mode(true, |gdb| {
            riscv::translate(satp, virtual_address, |address| {
                let bytes = gdb.read_memory(address, 8)?;
                Ok(u64::from_le_bytes(
                    bytes.try_into().expect("exact memory read"),
                ))
            })
        })
    }

    /// Best-effort register and stack diagnostics for the currently selected hart.
    pub fn diagnostics(&mut self) -> String {
        let mut out = String::new();
        for name in [
            "pc", "sp", "tp", "satp", "stvec", "scause", "sepc", "stval", "sstatus", "sie", "sip",
            "sscratch",
        ] {
            match self.read_register(name) {
                Ok(value) => out.push_str(&format!("{name}: {value:#018x}\n")),
                Err(err) => out.push_str(&format!("{name}: {err:#}\n")),
            }
        }
        match self.command("-stack-list-frames 0 31") {
            Ok(reply) => out.push_str(&format!("stack: {:#?}\n", reply.data)),
            Err(err) => out.push_str(&format!("stack: {err:#}\n")),
        }
        out
    }
}

#[derive(Debug, thiserror::Error)]
#[error("GDB: {0}")]
struct CommandError(String);

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("debugger paths must be UTF-8")
}

fn number(value: &str) -> Result<u64> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix("0x") {
        Ok(u64::from_str_radix(hex, 16)?)
    } else {
        Ok(value.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // A scripted MI peer exercises the real subprocess and pipe transport. It deliberately
    // sends a stop before the continue result and splits records across separate writes.
    fn peer() -> (tempfile::TempDir, Gdb) {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("fake-gdb");
        fs::write(&program, r##"#!/bin/sh
mode=0
stops=0
while IFS= read -r line; do
    token=${line%%-*}
    command=${line#"$token"}
    case "$command" in
        '-target-select '*)
            printf '*stopped,reason="signal-received",thread-id="1"\n'
            printf '%s^connected\n' "$token" ;;
        '-thread-info')
            printf '%s^done,threads=[{id="1",target-id="CPU#0",state="stopped"},{id="3",target-id="CPU#2",state="stopped"}]\n' "$token" ;;
        '-data-list-register-names')
            printf '%s^done,register-names=["pc","satp","scause","sepc","stval","sstatus","stvec"]\n' "$token" ;;
        '-data-list-register-values x 0')
            printf '%s^done,register-values=[{number="0",value="0xffffffc000001000"}]\n' "$token" ;;
        '-data-list-register-values x 2')
            printf '%s^done,register-values=[{number="2",value="0x8000000000000005"}]\n' "$token" ;;
        '-data-list-register-values x '*)
            printf '%s^done,register-values=[{number="1",value="0x0"}]\n' "$token" ;;
        '-break-insert '*)
            printf '%s^done,bkpt={number="2",type="hw breakpoint"}\n' "$token" ;;
        '-exec-continue --all')
            printf '*running,thread-id="all"\n'
            if [ "$stops" -eq 0 ]; then
                printf '*stopped,reason="breakpoint-hit",bkptno="2",thread-id="3",frame={addr="0xffffffc000001000"}\n'
            fi
            stops=1
            printf '%s^running\n' "$token" ;;
        '-exec-interrupt --all')
            printf '%s^done\n*stopped,reason="signal-received",signal-name="SIGINT",thread-id="3"\n' "$token" ;;
        *'qqemu.PhyMemMode'*)
            printf '~"received: \"%s\"\\n"\n%s^done\n' "$mode" "$token" ;;
        *'Qqemu.PhyMemMode:1'*)
            mode=1
            printf '~"received: \"OK\"\\n"\n%s^done\n' "$token" ;;
        *'Qqemu.PhyMemMode:0'*)
            mode=0
            printf '~"received: \"OK\"\\n"\n%s^done\n' "$token" ;;
        '-data-read-memory-bytes 0xdead 4')
            printf '%s^error,msg="unmapped memory"\n' "$token" ;;
        '-data-write-memory-bytes 0xdead '*)
            printf '%s^error,msg="unmapped memory"\n' "$token" ;;
        '-data-read-memory-bytes 0x1000 4')
            printf '%s^done,memory=[{begin="0x1000",end="0x1004",contents="' "$token"
            if [ "$mode" -eq 0 ]; then printf '01020304'; else printf 'aabbccdd'; fi
            printf '"}]\n' ;;
        '-data-read-memory-bytes 0x2000 4')
            printf '%s^done,memory=[{begin="0x2002",end="0x2004",contents="0102"}]\n' "$token" ;;
        *) printf '%s^done\n' "$token" ;;
    esac
done
"##).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let gdb = Gdb::connect(
            &program,
            &dir.path().join("unused"),
            dir.path(),
            dir.path(),
            Duration::from_secs(2),
        )
        .unwrap();
        (dir, gdb)
    }

    #[test]
    fn stop_before_reply_selects_the_stopping_hart() {
        let (dir, mut gdb) = peer();
        let bp = gdb.hardware_breakpoint(0xffff_ffc0_0000_1000).unwrap();
        let stop = gdb.run_to_breakpoint(&bp, Duration::from_secs(1)).unwrap();
        assert_eq!(stop.thread_id.as_deref(), Some("3"));
        assert_eq!(gdb.read_register("pc").unwrap(), 0xffff_ffc0_0000_1000);
        assert!(
            fs::read_to_string(dir.path().join("gdb-requests.log"))
                .unwrap()
                .contains("-thread-select 3")
        );
        let trap = gdb.trap_snapshot().unwrap();
        assert!(trap.is_interrupt());
        assert_eq!(trap.cause(), 5);
        assert_eq!(gdb.threads().unwrap().len(), 2);
    }

    #[test]
    fn memory_mode_restored_after_failed_read() {
        let (_dir, mut gdb) = peer();
        assert_eq!(
            gdb.read_physical(0x1000, 4).unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd]
        );
        assert!(gdb.read_physical(0xdead, 4).is_err());
        assert_eq!(gdb.packet("qqemu.PhyMemMode").unwrap(), "0");
        assert_eq!(gdb.read_virtual(0x1000, 4).unwrap(), [1, 2, 3, 4]);
        assert!(gdb.read_virtual(0x2000, 4).is_err());
    }

    #[test]
    fn timeout_interrupts_without_poisoning_session() {
        let (_dir, mut gdb) = peer();
        let bp = gdb.hardware_breakpoint(0x1000).unwrap();
        gdb.run_to_breakpoint(&bp, Duration::from_secs(1)).unwrap();
        let error = gdb
            .run_to_breakpoint(&bp, Duration::from_millis(30))
            .unwrap_err();
        assert!(error.is::<Timeout>(), "{error:#}");
        assert_eq!(gdb.state, State::Stopped);
        assert!(gdb.read_register("pc").is_ok());
    }

    #[test]
    fn virtual_write_restores_mode_and_rejects_invalid_ranges() {
        let (dir, mut gdb) = peer();
        gdb.packet("Qqemu.PhyMemMode:1").unwrap();
        gdb.write_virtual(0x1000, &[0, 0x12, 0xab, 0xff]).unwrap();
        assert_eq!(gdb.packet("qqemu.PhyMemMode").unwrap(), "1");
        assert!(gdb.write_virtual(0xdead, &[1]).is_err());
        assert_eq!(gdb.packet("qqemu.PhyMemMode").unwrap(), "1");
        assert!(gdb.write_virtual(u64::MAX, &[1]).is_err());
        assert!(gdb.write_virtual(0, &vec![0; 1024 * 1024 + 1]).is_err());
        let requests = fs::read_to_string(dir.path().join("gdb-requests.log")).unwrap();
        assert!(requests.contains("-data-write-memory-bytes 0x1000 0012abff"));
    }

    #[test]
    fn offsets_match_flat_image_composition() {
        let elf = ElfImage {
            path: "loader".into(),
            entry: 0x1100,
            minimum_load_address: 0x1000,
        };
        assert_eq!(
            elf.runtime_entry(SymbolMapping::LoadAt(0x8020_0000)),
            0x8020_0100
        );
        assert_eq!(elf.runtime_entry(SymbolMapping::Linked), 0x1100);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dropping_debugger_reaps_child() {
        let (_dir, gdb) = peer();
        let pid = gdb._child.0.id();
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        drop(gdb);
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }
}
