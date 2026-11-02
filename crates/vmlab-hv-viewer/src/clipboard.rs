//! Text clipboard bridge to the lab daemon's JSON-lines vocabulary.

use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub trait HostClipboard {
    fn get(&mut self) -> anyhow::Result<String>;
    fn set(&mut self, text: String) -> anyhow::Result<()>;
}

impl HostClipboard for arboard::Clipboard {
    fn get(&mut self) -> anyhow::Result<String> {
        Ok(self.get_text()?)
    }
    fn set(&mut self, text: String) -> anyhow::Result<()> {
        Ok(self.set_text(text)?)
    }
}

pub struct LabClipboard {
    pub socket: PathBuf,
    pub machine: String,
    id: u64,
    timeout: Duration,
}

impl LabClipboard {
    pub fn new(socket: impl AsRef<Path>, machine: String) -> Self {
        Self {
            socket: socket.as_ref().to_owned(),
            machine,
            id: 0,
            timeout: Duration::from_secs(2),
        }
    }

    fn request(&mut self, command: &str, args: Value) -> anyhow::Result<Value> {
        self.id += 1;
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        serde_json::to_writer(
            &mut stream,
            &json!({"type":"req", "id":self.id, "cmd":command, "args":args}),
        )?;
        stream.write_all(b"\n")?;
        let mut reader = BufReader::new(stream);
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
            reader.get_ref().set_read_timeout(Some(remaining))?;
            let mut line = String::new();
            let count = reader
                .by_ref()
                .take(16 * 1024 * 1024 + 1)
                .read_line(&mut line)?;
            anyhow::ensure!(
                count > 0 && count <= 16 * 1024 * 1024 && line.ends_with('\n'),
                "invalid clipboard reply length"
            );
            let reply: Value = serde_json::from_str(&line)?;
            if reply["type"] != "resp" || reply["id"] != self.id {
                continue;
            }
            if let Some(error) = reply["err"].as_str() {
                anyhow::bail!("lab clipboard: {error}");
            }
            return Ok(reply["ok"].clone());
        }
    }

    pub fn get(&mut self) -> anyhow::Result<String> {
        let reply = self.request("machine.clipboard_get", json!({"machine":self.machine}))?;
        Ok(reply
            .as_str()
            .ok_or_else(|| crate::invalid("clipboard reply is not text"))?
            .to_owned())
    }

    pub fn set(&mut self, text: &str) -> anyhow::Result<()> {
        self.request(
            "machine.clipboard_set",
            json!({"machine":self.machine,"text":text}),
        )?;
        Ok(())
    }
}

#[derive(Default)]
pub struct Bridge {
    focused: bool,
    last_host: Option<String>,
    last_poll: Option<Instant>,
    retry_at: Option<Instant>,
    retry_delay: Duration,
    reported: bool,
    recovered: bool,
}

impl Bridge {
    pub fn focus(&mut self, focused: bool) {
        if focused && !self.focused {
            self.last_host = None;
            self.last_poll = None;
        }
        self.focused = focused;
    }

    pub fn tick(
        &mut self,
        now: Instant,
        host: &mut impl HostClipboard,
        lab: &mut LabClipboard,
    ) -> anyhow::Result<()> {
        if !self.focused || self.retry_at.is_some_and(|when| now < when) {
            return Ok(());
        }
        let started = Instant::now();
        match self.synchronize(now, host, lab) {
            Ok(()) => {
                self.recovered |= self.reported;
                self.reported = false;
                self.retry_at = None;
                self.retry_delay = Duration::ZERO;
                Ok(())
            }
            Err(error) => {
                self.retry_delay = if self.retry_delay.is_zero() {
                    Duration::from_secs(1)
                } else {
                    (self.retry_delay * 2).min(Duration::from_secs(30))
                };
                self.retry_at = Some(now + started.elapsed() + self.retry_delay);
                if !temporary(&error) && !self.reported {
                    self.reported = true;
                    Err(error)
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn take_recovery(&mut self) -> bool {
        std::mem::take(&mut self.recovered)
    }

    fn synchronize(
        &mut self,
        now: Instant,
        host: &mut impl HostClipboard,
        lab: &mut LabClipboard,
    ) -> anyhow::Result<()> {
        // Host changes win if both selections changed between polls. Remember remote
        // writes too, so the next host read does not echo the same text to the guest.
        match host.get() {
            Ok(text) if self.last_host.as_ref() != Some(&text) => {
                lab.set(&text)?;
                self.last_host = Some(text);
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.downcast_ref::<arboard::Error>(),
                    Some(arboard::Error::ContentNotAvailable)
                ) => {}
            Err(error) => return Err(error),
        }
        if self
            .last_poll
            .is_none_or(|when| now.saturating_duration_since(when) >= Duration::from_secs(1))
        {
            let text = lab.get()?;
            if self.last_host.as_ref() != Some(&text) {
                host.set(text.clone())?;
                self.last_host = Some(text);
            }
            self.last_poll = Some(now);
        }
        Ok(())
    }
}

fn temporary(error: &anyhow::Error) -> bool {
    error.chain().any(|error| {
        if let Some(error) = error.downcast_ref::<io::Error>() {
            return matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            );
        }
        match error.downcast_ref::<arboard::Error>() {
            Some(arboard::Error::ClipboardOccupied) => true,
            // arboard's Wayland adapter erases the original Unix I/O error type.
            Some(arboard::Error::Unknown { description }) => {
                description == &io::Error::from_raw_os_error(libc::EAGAIN).to_string()
            }
            _ => false,
        }
    })
}

pub struct Worker {
    sender: Option<Sender<bool>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(mut lab: LabClipboard) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("viewer-clipboard".into())
            .spawn(move || {
                let mut host = match arboard::Clipboard::new() {
                    Ok(host) => host,
                    Err(error) => {
                        eprintln!("host clipboard unavailable: {error}");
                        return;
                    }
                };
                let mut bridge = Bridge::default();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(focused) => bridge.focus(focused),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if let Err(error) = bridge.tick(Instant::now(), &mut host, &mut lab) {
                        eprintln!("clipboard: {error}");
                    }
                    if bridge.take_recovery() {
                        eprintln!("clipboard synchronization resumed");
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    pub fn focus(&self, focused: bool) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(focused);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    struct Host {
        text: String,
        blocked: bool,
    }
    impl HostClipboard for Host {
        fn get(&mut self) -> anyhow::Result<String> {
            if self.blocked {
                return Err(io::Error::from(io::ErrorKind::WouldBlock).into());
            }
            Ok(self.text.clone())
        }
        fn set(&mut self, text: String) -> anyhow::Result<()> {
            self.text = text;
            Ok(())
        }
    }

    #[test]
    fn would_block_host_read_is_retried_quietly_before_touching_lab() {
        let directory = tempfile::tempdir().unwrap();
        let mut lab = LabClipboard::new(directory.path().join("absent.sock"), "vm".into());
        let mut host = Host {
            text: "host".into(),
            blocked: true,
        };
        let mut bridge = Bridge::default();
        bridge.focus(true);
        assert!(bridge.tick(Instant::now(), &mut host, &mut lab).is_ok());
    }

    #[test]
    fn lab_errors_back_off_and_alternating_messages_do_not_repeat_notices() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lab.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for error in [
                "no one is logged on to the desktop",
                "agent answered earlier but does not now",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                writeln!(
                    stream,
                    "{}",
                    json!({"type":"resp", "id":request["id"], "err":error})
                )
                .unwrap();
            }
        });
        let mut lab = LabClipboard::new(path, "vm".into());
        let mut host = Host {
            text: "host".into(),
            blocked: false,
        };
        let mut bridge = Bridge::default();
        bridge.focus(true);
        let now = Instant::now();
        assert!(bridge.tick(now, &mut host, &mut lab).is_err());
        assert!(
            bridge
                .tick(now + Duration::from_millis(100), &mut host, &mut lab)
                .is_ok()
        );
        assert!(
            bridge
                .tick(now + Duration::from_millis(1100), &mut host, &mut lab)
                .is_ok()
        );
        server.join().unwrap();
        assert_eq!(bridge.retry_delay, Duration::from_secs(2));
        for delay in [4, 8, 16, 30, 30] {
            let now = bridge.retry_at.unwrap() + Duration::from_millis(1);
            assert!(bridge.tick(now, &mut host, &mut lab).is_ok());
            assert_eq!(bridge.retry_delay, Duration::from_secs(delay));
        }
        bridge.focus(false);
        bridge.focus(true);
        assert!(bridge.tick(now, &mut host, &mut lab).is_ok());
        assert_eq!(bridge.retry_delay, Duration::from_secs(30));
    }

    #[test]
    fn lab_socket_timeout_is_quiet_and_success_reports_recovery_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lab.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                if attempt == 0 {
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                let reply = if attempt == 1 {
                    json!({"type":"resp", "id":request["id"], "err":"no one is logged on to the desktop"})
                } else {
                    json!({"type":"resp", "id":request["id"], "ok":if attempt == 3 { json!("host") } else { Value::Null }})
                };
                writeln!(stream, "{reply}").unwrap();
            }
        });
        let mut lab = LabClipboard::new(path, "vm".into());
        lab.timeout = Duration::from_millis(100);
        let mut bridge = Bridge::default();
        bridge.focus(true);
        let mut host = Host {
            text: "host".into(),
            blocked: false,
        };
        assert!(bridge.tick(Instant::now(), &mut host, &mut lab).is_ok());
        assert!(!bridge.reported);
        std::thread::sleep(Duration::from_millis(310));
        assert!(
            bridge
                .tick(
                    bridge.retry_at.unwrap() + Duration::from_millis(1),
                    &mut host,
                    &mut lab
                )
                .is_err()
        );
        assert!(
            bridge
                .tick(
                    bridge.retry_at.unwrap() + Duration::from_millis(1),
                    &mut host,
                    &mut lab
                )
                .is_ok()
        );
        assert!(bridge.take_recovery());
        assert!(!bridge.take_recovery());
        assert!(bridge.retry_at.is_none());
        server.join().unwrap();
        assert!(temporary(&anyhow::Error::from(arboard::Error::Unknown {
            description: io::Error::from_raw_os_error(libc::EAGAIN).to_string()
        })));
        assert!(temporary(&anyhow::Error::from(
            arboard::Error::ClipboardOccupied
        )));
    }
}
