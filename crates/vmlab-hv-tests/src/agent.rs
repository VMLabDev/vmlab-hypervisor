//! The host side of the vmlab-agent channel: framed, multiplexed byte
//! streams on the `vmlab.agent.0` virtio-serial port, exactly as vmlab's
//! `AgentHandle` drives it (`src/labd/vm_agent.rs`), on the wire types from
//! `vmlab-agent-proto`: a `Hello` handshake with a token the agent echoes,
//! then `open_exec` channels whose Data frames carry stdout.
//!
//! Sync and single-purpose: the harness needs connect, handshake and exec,
//! nothing else.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use vmlab_agent_proto::{
    AgentMsg, Frame, FrameDecoder, FrameKind, HostMsg, INITIAL_WINDOW, MAX_PAYLOAD, PROTO_VERSION,
    RecvWindow, SendWindow, encode_ctrl, encode_frame,
};

/// What the agent said about itself in the handshake.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub proto_version: u32,
    pub agent_version: String,
    pub os: String,
    pub features: Vec<String>,
}

/// One collected exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Errors the harness cares about, as strings: every failure ends a test.
pub type Error = String;

/// A sync client for one guest's agent.
pub struct AgentClient {
    stream: UnixStream,
    decoder: FrameDecoder,
    info: AgentInfo,
    next_channel: u32,
    send_windows: HashMap<u32, SendWindow>,
    recv_windows: HashMap<u32, RecvWindow>,
}

/// A random-ish handshake token, as vmlab mints one: any replay barrier the
/// agent has not seen before will do.
fn token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{nanos:016x}")
}

impl AgentClient {
    /// Connect and handshake, retrying while the guest's agent is still
    /// starting: each attempt is a fresh connection with a fresh token, as a
    /// host reconnecting after a dropped channel would.
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self, Error> {
        let deadline = Instant::now() + timeout;
        let mut last = "no attempt made".to_string();
        while Instant::now() < deadline {
            let attempt_timeout = deadline.saturating_duration_since(Instant::now());
            match Self::attempt(path, attempt_timeout.min(Duration::from_secs(3))) {
                Ok(client) => return Ok(client),
                Err(e) => {
                    last = e;
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
        Err(format!(
            "no vmlab-agent answered on {}: {last}",
            path.display()
        ))
    }

    fn attempt(path: &Path, timeout: Duration) -> Result<Self, Error> {
        let stream =
            UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| format!("read timeout: {e}"))?;
        let mut client = AgentClient {
            stream,
            decoder: FrameDecoder::new(),
            info: AgentInfo {
                proto_version: 0,
                agent_version: String::new(),
                os: String::new(),
                features: Vec::new(),
            },
            next_channel: 1,
            send_windows: HashMap::new(),
            recv_windows: HashMap::new(),
        };
        let token = token();
        client.write_ctrl(&HostMsg::Hello {
            proto_version: PROTO_VERSION,
            token: token.clone(),
        })?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("agent did not answer the handshake".to_string());
            }
            let Some(frame) = client.read_frame(remaining)? else {
                continue;
            };
            if frame.kind != FrameKind::Ctrl {
                continue;
            }
            let Ok(AgentMsg::Hello {
                proto_version,
                agent_version,
                os,
                features,
                token: echoed,
                ..
            }) = serde_json::from_slice(&frame.payload)
            else {
                continue;
            };
            if echoed != token {
                return Err(format!(
                    "agent echoed the wrong token ({echoed}); stale replay?"
                ));
            }
            if proto_version != PROTO_VERSION {
                return Err(format!(
                    "agent speaks proto v{proto_version}, harness speaks v{PROTO_VERSION}"
                ));
            }
            client.info = AgentInfo {
                proto_version,
                agent_version,
                os,
                features,
            };
            return Ok(client);
        }
    }

    pub fn info(&self) -> &AgentInfo {
        &self.info
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.stream
            .write_all(bytes)
            .map_err(|e| format!("write: {e}"))
    }

    fn write_ctrl(&mut self, msg: &HostMsg) -> Result<(), Error> {
        let bytes = encode_ctrl(msg);
        self.write_all(&bytes)
    }

    /// Read one frame, or `None` at this call's deadline. Ctrl traffic is
    /// handled inline: window grants are applied, everything else is handed
    /// back to the caller as a raw frame.
    fn read_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, Error> {
        let deadline = Instant::now() + timeout;
        let mut chunk = [0u8; 16 * 1024];
        loop {
            if let Some(frame) = self.decoder.next_frame() {
                if frame.kind == FrameKind::Ctrl
                    && let Ok(AgentMsg::WindowAdjust { id, bytes }) =
                        serde_json::from_slice(&frame.payload)
                    && let Some(window) = self.send_windows.get_mut(&id)
                {
                    window.grant(bytes);
                    continue;
                }
                return Ok(Some(frame));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("agent port closed".to_string()),
                Ok(n) => self.decoder.push(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(format!("read: {e}")),
            }
        }
    }

    /// Run `argv` to completion and collect stdout/stderr/exit code.
    pub fn exec(&mut self, argv: &[&str], timeout: Duration) -> Result<ExecOutput, Error> {
        let channel = self.next_channel;
        self.next_channel += 1;
        self.send_windows
            .insert(channel, SendWindow::new(INITIAL_WINDOW));
        self.recv_windows.insert(channel, RecvWindow::default());

        self.write_ctrl(&HostMsg::OpenExec {
            id: channel,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            cwd: None,
            logon: None,
            tree: None,
        })?;

        let deadline = Instant::now() + timeout;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(frame) = self.read_frame(remaining)? else {
                return Err(format!("exec `{}` timed out", argv.join(" ")));
            };
            match frame.kind {
                FrameKind::Ctrl => {
                    let msg: AgentMsg = serde_json::from_slice(&frame.payload)
                        .map_err(|e| format!("bad ctrl frame: {e}"))?;
                    match msg {
                        AgentMsg::Opened { id: _ } => {}
                        AgentMsg::Eof { id: _ } => {}
                        AgentMsg::Exited { id, code } if id == channel => {
                            // Drain done: tell the agent it may drop the
                            // channel.
                            let _ = self.write_ctrl(&HostMsg::Close { id: channel });
                            self.send_windows.remove(&channel);
                            self.recv_windows.remove(&channel);
                            return Ok(ExecOutput {
                                exit_code: code,
                                stdout,
                                stderr,
                            });
                        }
                        AgentMsg::Error { id: Some(id), msg } if id == channel => {
                            return Err(format!("exec `{}` failed: {msg}", argv.join(" ")));
                        }
                        AgentMsg::Error { msg, .. } => {
                            return Err(format!("agent error: {msg}"));
                        }
                        _ => {}
                    }
                }
                FrameKind::Data if frame.channel == channel => {
                    stdout.extend_from_slice(&frame.payload);
                    self.replenish(channel, frame.payload.len())?;
                }
                FrameKind::DataErr if frame.channel == channel => {
                    stderr.extend_from_slice(&frame.payload);
                    self.replenish(channel, frame.payload.len())?;
                }
                _ => {}
            }
        }
    }

    /// Send stdin bytes on an exec channel (credit-checked), then EOF.
    pub fn send_stdin(&mut self, channel: u32, bytes: &[u8]) -> Result<(), Error> {
        for chunk in bytes.chunks(MAX_PAYLOAD) {
            {
                let window = self
                    .send_windows
                    .get_mut(&channel)
                    .ok_or_else(|| format!("channel {channel} has no window"))?;
                if (chunk.len() as u64) > window.credit() {
                    return Err(format!(
                        "channel {channel} out of credit ({} bytes wanted, {} left)",
                        chunk.len(),
                        window.credit()
                    ));
                }
                window.consume(chunk.len());
            }
            let frame = encode_frame(FrameKind::Data, channel, chunk);
            self.write_all(&frame)?;
        }
        self.write_ctrl(&HostMsg::Eof { id: channel })
    }

    /// Grant the guest more credit once it has sent enough bytes.
    fn replenish(&mut self, channel: u32, len: usize) -> Result<(), Error> {
        let Some(window) = self.recv_windows.get_mut(&channel) else {
            return Ok(());
        };
        if let Some(bytes) = window.recv(len) {
            self.write_ctrl(&HostMsg::WindowAdjust { id: channel, bytes })?;
        }
        Ok(())
    }
}
