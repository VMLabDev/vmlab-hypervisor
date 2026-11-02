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
}

impl LabClipboard {
    pub fn new(socket: impl AsRef<Path>, machine: String) -> Self {
        Self {
            socket: socket.as_ref().to_owned(),
            machine,
            id: 0,
        }
    }

    fn request(&mut self, command: &str, args: Value) -> anyhow::Result<Value> {
        self.id += 1;
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        serde_json::to_writer(
            &mut stream,
            &json!({"type":"req", "id":self.id, "cmd":command, "args":args}),
        )?;
        stream.write_all(b"\n")?;
        let mut reader = BufReader::new(stream);
        loop {
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
        if !self.focused {
            return Ok(());
        }
        // Host changes win if both selections changed between polls. Remember remote
        // writes too, so the next host read does not echo the same text to the guest.
        if let Ok(text) = host.get()
            && self.last_host.as_ref() != Some(&text)
        {
            lab.set(&text)?;
            self.last_host = Some(text);
        }
        if self
            .last_poll
            .is_none_or(|when| now.saturating_duration_since(when) >= Duration::from_secs(1))
        {
            self.last_poll = Some(now);
            let text = lab.get()?;
            if self.last_host.as_ref() != Some(&text) {
                host.set(text.clone())?;
                self.last_host = Some(text);
            }
        }
        Ok(())
    }
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
                let mut last_error = String::new();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(focused) => bridge.focus(focused),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if let Err(error) = bridge.tick(Instant::now(), &mut host, &mut lab) {
                        let error = error.to_string();
                        if error != last_error {
                            eprintln!("clipboard: {error}");
                            last_error = error;
                        }
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
