use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};
use anyhow::{Context, Result};

/// UART input plus continuous byte-for-byte output capture, independent of debugger stops.
pub struct Serial {
    socket: UnixStream,
    tail: Arc<Mutex<VecDeque<u8>>>,
    reader: Option<JoinHandle<std::io::Result<()>>>,
}

impl Serial {
    pub(super) fn connect(socket: UnixStream, mut log: File, timeout: Duration) -> Result<Self> {
        socket.set_write_timeout(Some(timeout))?;
        let mut input = socket.try_clone()?;
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let output_tail = tail.clone();
        let reader = thread::spawn(move || {
            let mut bytes = [0; 4096];
            loop {
                let count = input.read(&mut bytes)?;
                if count == 0 {
                    return Ok(());
                }
                log.write_all(&bytes[..count])?;
                let mut tail = output_tail.lock().unwrap();
                tail.extend(&bytes[..count]);
                while tail.len() > 64 * 1024 {
                    tail.pop_front();
                }
            }
        });
        Ok(Self {
            socket,
            tail,
            reader: Some(reader),
        })
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.check()?;
        self.socket.write_all(bytes).context("send UART input")
    }

    pub fn tail(&self) -> Vec<u8> {
        self.tail.lock().unwrap().iter().copied().collect()
    }

    /// Report background capture failures. Full output is always stored in serial.log.
    pub fn check(&mut self) -> Result<()> {
        if self.reader.as_ref().is_some_and(JoinHandle::is_finished) {
            self.reader
                .take()
                .unwrap()
                .join()
                .map_err(|_| anyhow::anyhow!("serial reader panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Serial {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
