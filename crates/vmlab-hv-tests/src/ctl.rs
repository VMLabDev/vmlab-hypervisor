//! The host side of the cinit ctl channel: newline-delimited JSON on the
//! `vmlab.ctl.0` virtio-serial port, exactly as vmlab drives it today
//! (`src/labd/container.rs`): send the `Spec` once the socket is connectable,
//! re-answer every repeated `boot` announcement, and watch for lifecycle
//! events.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use vmlab_cinit_proto::{ContainerSpec, CtlCommand, CtlEvent};

use crate::connect_with_retry;

/// What a read produced: an event, or the port going away (the machine is
/// exiting).
pub enum CtlRead {
    Event(CtlEvent),
    Closed,
}

/// One side of the ctl channel. Reads events with timeouts; writes commands.
pub struct CtlClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl CtlClient {
    /// Connect to the hypervisor's ctl port listener. The listener exists
    /// from machine build, before the control socket is even served.
    pub fn connect(path: &Path, timeout: Duration) -> std::io::Result<Self> {
        let stream = connect_with_retry(path, timeout)?;
        let writer = stream.try_clone()?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(CtlClient {
            reader: BufReader::new(stream),
            writer,
        })
    }

    pub fn send(&mut self, cmd: &CtlCommand) -> std::io::Result<()> {
        let mut line = serde_json::to_string(cmd).expect("ctl command serializes");
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()
    }

    /// Send the container spec (the first thing vmlab does after connecting).
    pub fn send_spec(&mut self, spec: &ContainerSpec) -> std::io::Result<()> {
        self.send(&CtlCommand::Spec {
            spec: Box::new(spec.clone()),
        })
    }

    /// Block for the next event, `Closed` when the port went away, or `None`
    /// at the deadline. Repeated `boot` announcements are answered with a
    /// fresh spec, as vmlab's watcher does.
    pub fn next_event(&mut self, spec: &ContainerSpec, timeout: Duration) -> Option<CtlRead> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let mut line = String::new();
            // The read timeout is the polling granularity; EOF fails the read.
            match self.reader.read_line(&mut line) {
                Ok(0) => return Some(CtlRead::Closed),
                Ok(_) => {
                    let Ok(event) = serde_json::from_str::<CtlEvent>(line.trim()) else {
                        // Unknown line (proto drift): skip it, the deadline
                        // still bounds the wait.
                        continue;
                    };
                    if matches!(event, CtlEvent::Boot { .. }) {
                        let _ = self.send_spec(spec);
                    }
                    return Some(CtlRead::Event(event));
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    continue;
                }
                Err(_) => return Some(CtlRead::Closed),
            }
        }
    }

    /// Drive until `want` returns true for an event; other events (other
    /// than `boot`) are passed to `on` first. `Boot` is always swallowed
    /// (re-answered). Panics at the deadline or if the port closes.
    pub fn wait_event(
        &mut self,
        spec: &ContainerSpec,
        timeout: Duration,
        want: impl Fn(&CtlEvent) -> bool,
        mut on: impl FnMut(&CtlEvent),
        diagnostic: impl FnOnce() -> String,
    ) -> CtlEvent {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(read) = self.next_event(spec, remaining) else {
                panic!("ctl channel never produced the wanted event: {}", diagnostic());
            };
            match read {
                CtlRead::Closed => panic!("ctl port closed early: {}", diagnostic()),
                CtlRead::Event(event) => {
                    if matches!(event, CtlEvent::Boot { .. }) {
                        continue;
                    }
                    if want(&event) {
                        return event;
                    }
                    on(&event);
                }
            }
        }
    }
}
