use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::Child,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

pub(super) struct ChildGuard(pub Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug, thiserror::Error)]
#[error("timed out waiting for {0}")]
pub struct Timeout(pub &'static str);

pub(super) fn remaining(deadline: Instant, what: &'static str) -> Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        return Err(Timeout(what).into());
    }
    Ok(duration)
}

pub(super) struct Lines(Receiver<io::Result<String>>);

impl Lines {
    pub fn spawn(reader: impl Read + Send + 'static, mut log: Option<File>) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                let mut bytes = Vec::new();
                let result = reader
                    .by_ref()
                    .take(16 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut bytes);
                let result = match result {
                    Ok(0) => break,
                    Ok(n) if n > 16 * 1024 * 1024 => {
                        Err(io::Error::other("protocol record exceeds 16 MiB"))
                    }
                    Ok(_) => {
                        if let Some(log) = log.as_mut()
                            && let Err(err) = log.write_all(&bytes)
                        {
                            let _ = tx.send(Err(err));
                            break;
                        }
                        String::from_utf8(bytes)
                            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
                    }
                    Err(err) => Err(err),
                };
                let failed = result.is_err();
                if tx.send(result).is_err() || failed {
                    break;
                }
            }
        });
        Self(rx)
    }

    pub fn recv(&self, deadline: Instant, what: &'static str) -> Result<String> {
        match self.0.recv_timeout(remaining(deadline, what)?) {
            Ok(line) => line.context(what),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Timeout(what).into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("{what}: connection closed"),
        }
    }
}

pub(super) fn connect_socket(
    path: &Path,
    child: &mut ChildGuard,
    deadline: Instant,
) -> Result<UnixStream> {
    loop {
        match UnixStream::connect(path) {
            Ok(socket) => return Ok(socket),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(err) => return Err(err).with_context(|| format!("connect to {}", path.display())),
        }
        if let Some(status) = child.0.try_wait()? {
            bail!("QEMU exited during startup: {status}");
        }
        thread::sleep(remaining(deadline, "QEMU sockets")?.min(Duration::from_millis(10)));
    }
}
