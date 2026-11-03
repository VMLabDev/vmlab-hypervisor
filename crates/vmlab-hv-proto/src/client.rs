//! The client side of the control protocol: what vmlab and the S1 test
//! harness speak.
//!
//! [`Client::connect`] performs the version handshake and records the
//! server's hello ([`Client::proto_version`], [`Client::hv_version`],
//! [`Client::capabilities`]). [`Client::call`] sends one command and blocks
//! for its reply by id, buffering any [`Event`]s that arrive first;
//! [`Client::take_events`] hands those over and [`Client::wait_event`]
//! blocks for the next one, returning `Ok(None)` once the server closes the
//! stream.

use std::collections::VecDeque;
use std::io::BufReader;
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;

use crate::PROTO_VERSION;
use crate::codec::{self, CodecError};
use crate::control::{Capability, ClientMsg, Command, ErrorCode, Event, Outcome, ServerMsg};

/// A connection to one hypervisor's control socket.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    proto_version: u32,
    hv_version: String,
    capabilities: Vec<Capability>,
    events: VecDeque<Event>,
}

/// Failures of a client session.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Codec(#[from] CodecError),
    /// The server refused the version handshake; `code` is
    /// [`ErrorCode::VersionMismatch`] in practice.
    #[error("handshake rejected: {code:?}: {message}")]
    HandshakeRejected { code: ErrorCode, message: String },
    /// The server closed the stream before answering.
    #[error("server closed the connection")]
    Closed,
    /// The server sent a message no client flow can produce.
    #[error("unexpected message from server: {0:?}")]
    Unexpected(Box<ServerMsg>),
    /// The server answered an id this client did not ask about.
    #[error("reply id {got} does not match the request id {expected}")]
    ReplyMismatch { expected: u64, got: u64 },
}

impl Client {
    /// Connect to the control socket at `path` and perform the version
    /// handshake.
    pub fn connect(path: impl AsRef<std::path::Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path.as_ref())?;
        Self::handshake(stream)
    }

    fn handshake(stream: UnixStream) -> Result<Self, ClientError> {
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        codec::write_client_msg(
            &mut writer,
            &ClientMsg::Hello {
                proto_version: PROTO_VERSION,
            },
        )?;
        let (proto_version, hv_version, capabilities) = match codec::read_server_msg(&mut reader)? {
            Some(ServerMsg::Hello {
                proto_version,
                hv_version,
                capabilities,
            }) => {
                if proto_version != PROTO_VERSION {
                    return Err(ClientError::HandshakeRejected {
                        code: ErrorCode::VersionMismatch,
                        message: format!(
                            "server protocol version {proto_version}, client speaks {PROTO_VERSION}"
                        ),
                    });
                }
                (proto_version, hv_version, capabilities)
            }
            Some(ServerMsg::Error { code, message }) => {
                return Err(ClientError::HandshakeRejected { code, message });
            }
            Some(other) => return Err(ClientError::Unexpected(Box::new(other))),
            None => return Err(ClientError::Closed),
        };
        Ok(Self {
            reader,
            writer,
            next_id: 1,
            proto_version,
            hv_version,
            capabilities,
            events: VecDeque::new(),
        })
    }

    /// The protocol version the server confirmed in the handshake.
    pub fn proto_version(&self) -> u32 {
        self.proto_version
    }

    /// The hypervisor's own version, as reported in the handshake.
    pub fn hv_version(&self) -> &str {
        &self.hv_version
    }

    /// The capabilities the server advertised, including unknown names for
    /// logging. Unknown capabilities do not enable any known feature.
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    /// Whether the server advertised `capability`.
    pub fn has_capability(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Send `command` and block for its reply. Events that arrive first are
    /// buffered; see [`Client::take_events`] and [`Client::wait_event`].
    pub fn call(&mut self, command: Command) -> Result<Outcome, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        codec::write_client_msg(&mut self.writer, &ClientMsg::Request { id, command })?;
        self.await_reply(id)
    }

    /// Send one request with borrowed descriptors. Ownership stays with the caller.
    /// For `NetAttach`, pass TAPs in queue-pair order, then matching vhost fds.
    pub fn call_with_fds(
        &mut self,
        command: Command,
        fds: &[BorrowedFd<'_>],
    ) -> Result<Outcome, ClientError> {
        crate::fd::validate_count(&command, fds.len())?;
        let id = self.next_id;
        self.next_id += 1;
        crate::fd::write_client_msg(&self.writer, &ClientMsg::Request { id, command }, fds)?;
        self.await_reply(id)
    }

    fn await_reply(&mut self, id: u64) -> Result<Outcome, ClientError> {
        loop {
            match codec::read_server_msg(&mut self.reader)? {
                None => return Err(ClientError::Closed),
                Some(ServerMsg::Event { event }) => self.events.push_back(event),
                Some(ServerMsg::Reply { id: got, outcome }) => {
                    return if got == id {
                        Ok(outcome)
                    } else {
                        Err(ClientError::ReplyMismatch { expected: id, got })
                    };
                }
                Some(other) => return Err(ClientError::Unexpected(Box::new(other))),
            }
        }
    }

    /// Block for the next event, serving buffered events first. `Ok(None)`
    /// means the server closed the stream. [`Event::Unknown`] is returned
    /// with its name so callers can log and ignore it without ending the stream.
    pub fn wait_event(&mut self) -> Result<Option<Event>, ClientError> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        match codec::read_server_msg(&mut self.reader)? {
            Some(ServerMsg::Event { event }) => Ok(Some(event)),
            None => Ok(None),
            Some(other) => Err(ClientError::Unexpected(Box::new(other))),
        }
    }

    /// [`Client::wait_event`] with a deadline: `Ok(None)` also covers "the
    /// deadline passed first", so callers cannot tell a timeout from a
    /// closed stream — both mean no more events will arrive in time.
    pub fn wait_event_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<Event>, ClientError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.reader
                .get_ref()
                .set_read_timeout(Some(remaining.min(std::time::Duration::from_millis(250))))?;
            match codec::read_server_msg(&mut self.reader) {
                Ok(Some(ServerMsg::Event { event })) => return Ok(Some(event)),
                Ok(None) => return Ok(None),
                Ok(Some(other)) => return Err(ClientError::Unexpected(Box::new(other))),
                Err(CodecError::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Take the events buffered while waiting for command replies.
    pub fn take_events(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;

    fn socket_path(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("hv-client-{name}-"))
            .tempdir()
            .unwrap();
        let path = dir.path().join("control.sock");
        (dir, path)
    }

    #[test]
    fn handshake_reports_the_server_hello() {
        let (_dir, path) = socket_path("hello");
        let server = Server::new("0.42.0", vec![Capability::Snapshots], |_| Outcome::Ok {
            status: None,
        });
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        thread::spawn(move || server.serve(listener).unwrap());

        let client = Client::connect(&path).unwrap();
        assert_eq!(client.proto_version(), PROTO_VERSION);
        assert_eq!(client.hv_version(), "0.42.0");
        assert_eq!(client.capabilities(), &[Capability::Snapshots]);
        assert!(client.has_capability(Capability::Snapshots));
        assert!(!client.has_capability(Capability::Display));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn server_that_closes_after_our_hello_is_reported_closed() {
        let (_dir, path) = socket_path("closed");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Consume the client's hello, then close gracefully: the client
            // sees EOF rather than a reset.
            let mut reader = BufReader::new(stream);
            assert!(codec::read_client_msg(&mut reader).unwrap().is_some());
        });
        let err = Client::connect(&path).unwrap_err();
        assert!(matches!(err, ClientError::Closed));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn call_returns_the_reply_and_buffers_events() {
        let (_dir, path) = socket_path("call");
        let (saw_request, request_seen) = mpsc::channel::<()>();
        let (event_sent, send_event) = mpsc::channel::<()>();
        let server = Server::new("test-hv", vec![], move |command| {
            if command == Command::Pause {
                saw_request.send(()).unwrap();
                // Hold the reply until the event is on the wire.
                send_event.recv().unwrap();
            }
            Outcome::Ok {
                status: Some(crate::control::RunState::Paused),
            }
        });
        let broadcaster = server.clone();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        thread::spawn(move || server.serve(listener).unwrap());
        thread::spawn(move || {
            request_seen.recv().unwrap();
            broadcaster.broadcast(Event::Stopped);
            broadcaster.broadcast(Event::Resumed);
            event_sent.send(()).unwrap();
        });

        let mut client = Client::connect(&path).unwrap();
        let outcome = client.call(Command::Pause).unwrap();
        assert_eq!(
            outcome,
            Outcome::Ok {
                status: Some(crate::control::RunState::Paused),
            }
        );
        // Both events pushed while the request was served arrived before the
        // reply and were buffered for us.
        assert_eq!(client.take_events(), vec![Event::Stopped, Event::Resumed]);
        assert!(client.take_events().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fd_call_keeps_descriptor_order_and_buffers_events_before_its_reply() {
        use std::io::{Read, Write};
        use std::os::fd::AsFd;
        use std::time::Duration;

        let (_dir, path) = socket_path("fds");
        let (saw_request, request_seen) = mpsc::channel();
        let (event_sent, send_event) = mpsc::channel();
        let server = Server::with_fds("test-hv", vec![Capability::NetFds], move |command, fds| {
            match command {
                Command::NetAttach {
                    mac,
                    queue_pairs,
                    vhost,
                } => {
                    assert_eq!(mac, "52:54:00:12:34:56");
                    assert_eq!(queue_pairs, 1);
                    assert!(vhost);
                    assert_eq!(fds.len(), 2);
                    for (fd, expected) in fds.into_iter().zip(*b"tv") {
                        assert!(
                            rustix::io::fcntl_getfd(&fd)
                                .unwrap()
                                .contains(rustix::io::FdFlags::CLOEXEC)
                        );
                        let mut passed = UnixStream::from(fd);
                        let mut byte = [0];
                        passed.read_exact(&mut byte).unwrap();
                        assert_eq!(byte[0], expected);
                    }
                    saw_request.send(()).unwrap();
                    send_event.recv().unwrap();
                }
                Command::Status => assert!(fds.is_empty()),
                other => panic!("unexpected command {other:?}"),
            }
            Outcome::Ok { status: None }
        });
        let broadcaster = server.clone();
        let owner = server.clone();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || server.serve(listener).unwrap());
        let events = thread::spawn(move || {
            request_seen.recv().unwrap();
            broadcaster.broadcast(Event::Stopped);
            event_sent.send(()).unwrap();
        });
        let mut client = Client::connect(&path).unwrap();
        client
            .reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (tap, mut tap_peer) = UnixStream::pair().unwrap();
        let (vhost, mut vhost_peer) = UnixStream::pair().unwrap();
        tap_peer.write_all(b"t").unwrap();
        vhost_peer.write_all(b"v").unwrap();
        assert_eq!(
            client
                .call_with_fds(
                    Command::NetAttach {
                        mac: "52:54:00:12:34:56".into(),
                        queue_pairs: 1,
                        vhost: true,
                    },
                    &[tap.as_fd(), vhost.as_fd()]
                )
                .unwrap(),
            Outcome::Ok { status: None }
        );
        assert_eq!(client.take_events(), vec![Event::Stopped]);
        assert_eq!(
            client.call(Command::Status).unwrap(),
            Outcome::Ok { status: None }
        );
        owner.shutdown();
        events.join().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn invalid_fd_counts_do_not_consume_a_request_id_or_write_a_request() {
        use std::os::fd::AsFd;
        let (_dir, path) = socket_path("bad-fds");
        let server = Server::new("test-hv", vec![], |command| {
            assert_eq!(command, Command::Status);
            Outcome::Ok { status: None }
        });
        let owner = server.clone();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || server.serve(listener).unwrap());
        let mut client = Client::connect(&path).unwrap();
        let (fd, _peer) = UnixStream::pair().unwrap();
        assert!(
            client
                .call_with_fds(Command::Status, &[fd.as_fd()])
                .is_err()
        );
        assert_eq!(client.next_id, 1);
        assert_eq!(
            client.call(Command::Status).unwrap(),
            Outcome::Ok { status: None }
        );
        owner.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn wait_event_serves_an_event_then_reports_the_close() {
        let (_dir, path) = socket_path("wait");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let hello = codec::read_client_msg(&mut reader).unwrap().unwrap();
            assert!(matches!(hello, ClientMsg::Hello { .. }));
            codec::write_server_msg(
                &mut writer,
                &ServerMsg::Hello {
                    proto_version: PROTO_VERSION,
                    hv_version: "short-lived".to_owned(),
                    capabilities: vec![],
                },
            )
            .unwrap();
            codec::write_server_msg(
                &mut writer,
                &ServerMsg::Event {
                    event: Event::Suspend,
                },
            )
            .unwrap();
            // Then the server goes away.
        });

        let mut client = Client::connect(&path).unwrap();
        assert_eq!(client.wait_event().unwrap(), Some(Event::Suspend));
        assert_eq!(client.wait_event().unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wait_event_drains_buffered_events_first() {
        // Events buffered during a call come out of wait_event in order
        // without touching the socket.
        let mut client = Client {
            reader: BufReader::new(pair_half()),
            writer: pair_half(),
            next_id: 1,
            proto_version: PROTO_VERSION,
            hv_version: String::new(),
            capabilities: Vec::new(),
            events: VecDeque::from(vec![Event::Reset, Event::Wakeup]),
        };
        assert_eq!(client.wait_event().unwrap(), Some(Event::Reset));
        assert_eq!(client.wait_event().unwrap(), Some(Event::Wakeup));
        assert!(client.take_events().is_empty());
    }

    /// An unconnected socket pair, for constructing a client without a
    /// server.
    fn pair_half() -> UnixStream {
        UnixStream::pair().unwrap().0
    }

    #[test]
    fn reply_with_the_wrong_id_is_reported() {
        let (_dir, path) = socket_path("mismatch");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            // Handshake.
            let hello = codec::read_client_msg(&mut reader).unwrap().unwrap();
            assert!(matches!(hello, ClientMsg::Hello { .. }));
            codec::write_server_msg(
                &mut writer,
                &ServerMsg::Hello {
                    proto_version: PROTO_VERSION,
                    hv_version: "misbehaving".to_owned(),
                    capabilities: vec![],
                },
            )
            .unwrap();
            // Answer the request with a bogus id.
            let request = codec::read_client_msg(&mut reader).unwrap().unwrap();
            let ClientMsg::Request { id, .. } = request else {
                panic!("expected a request, got {request:?}");
            };
            codec::write_server_msg(
                &mut writer,
                &ServerMsg::Reply {
                    id: id + 10,
                    outcome: Outcome::Ok { status: None },
                },
            )
            .unwrap();
        });

        let mut client = Client::connect(&path).unwrap();
        let err = client.call(Command::Status).unwrap_err();
        assert_eq!(
            err.to_string(),
            "reply id 11 does not match the request id 1"
        );
        assert!(matches!(
            err,
            ClientError::ReplyMismatch {
                expected: 1,
                got: 11,
            }
        ));
        let _ = std::fs::remove_file(&path);
    }
}
