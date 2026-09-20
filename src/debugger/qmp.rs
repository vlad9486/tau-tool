use std::{
    fs::File,
    io::Write,
    net::Shutdown,
    os::unix::net::UnixStream,
    time::{Duration, Instant},
    collections::VecDeque,
};

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

use super::transport::Lines;

/// QMP lifecycle channel. Execution control belongs to GDB while it is attached.
pub struct Qmp {
    socket: UnixStream,
    lines: Lines,
    next_id: u64,
    timeout: Duration,
    events: VecDeque<Value>,
    healthy: bool,
}

impl Qmp {
    pub(super) fn connect(socket: UnixStream, log: File, timeout: Duration) -> Result<Self> {
        socket.set_write_timeout(Some(timeout))?;
        let lines = Lines::spawn(socket.try_clone()?, Some(log));
        let mut qmp = Self {
            socket,
            lines,
            next_id: 1,
            timeout,
            events: VecDeque::new(),
            healthy: true,
        };
        let greeting: Value =
            serde_json::from_str(&qmp.lines.recv(Instant::now() + timeout, "QMP greeting")?)?;
        ensure!(
            greeting.get("QMP").is_some(),
            "invalid QMP greeting: {greeting}"
        );
        qmp.execute("qmp_capabilities", json!({}))?;
        Ok(qmp)
    }

    pub fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
        ensure!(
            self.healthy,
            "QMP transport is unusable; create a new session"
        );
        let id = self.next_id;
        self.next_id += 1;
        // Mark unhealthy until a matching response is consumed, including error responses.
        self.healthy = false;
        let request = json!({"execute": command, "arguments": arguments, "id": id});
        writeln!(self.socket, "{request}\r")?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let value: Value = serde_json::from_str(&self.lines.recv(deadline, "QMP response")?)?;
            if value.get("event").is_some() {
                if self.events.len() == 128 {
                    self.events.pop_front();
                }
                self.events.push_back(value);
                continue;
            }
            ensure!(
                value.get("id").and_then(Value::as_u64) == Some(id),
                "unexpected QMP response ID: {value}"
            );
            self.healthy = true;
            if let Some(error) = value.get("error") {
                bail!("QMP {command}: {error}");
            }
            return value
                .get("return")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("invalid QMP response: {value}"));
        }
    }

    pub fn status(&mut self) -> Result<Value> {
        self.execute("query-status", json!({}))
    }
    pub fn quit(&mut self) -> Result<()> {
        self.execute("quit", json!({}))?;
        Ok(())
    }

    /// Drain notifications seen while reading command responses (last 128 retained).
    pub fn drain_events(&mut self) -> impl Iterator<Item = Value> + '_ {
        self.events.drain(..)
    }
}

impl Drop for Qmp {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        thread,
    };

    #[test]
    fn handshake_and_interleaved_event_and_error() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let peer = thread::spawn(move || {
            writeln!(
                server,
                "{}",
                json!({"QMP": {"version": {}, "capabilities": []}})
            )
            .unwrap();
            let mut input = BufReader::new(server.try_clone().unwrap());
            for index in 1..=3 {
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["id"], index);
                if index == 1 {
                    assert_eq!(request["execute"], "qmp_capabilities");
                }
                writeln!(server, "{}", json!({"event": "STOP"})).unwrap();
                let reply = if index == 2 {
                    json!({"id": index, "error": {"class": "GenericError", "desc": "failure"}})
                } else {
                    json!({"id": index, "return": {"status": "paused"}})
                };
                writeln!(server, "{reply}").unwrap();
            }
        });
        let mut qmp = Qmp::connect(
            client,
            tempfile::tempfile().unwrap(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(qmp.status().is_err());
        assert_eq!(qmp.status().unwrap()["status"], "paused");
        assert_eq!(qmp.drain_events().count(), 3);
        peer.join().unwrap();
    }
}
