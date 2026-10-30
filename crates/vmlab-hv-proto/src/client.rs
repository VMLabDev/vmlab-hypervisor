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

    /// The capabilities the server advertised in the handshake.
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
    /// means the server closed the stream.
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

    fn socket_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vmlab-hv-proto-client-{name}-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn handshake_reports_the_server_hello() {
        let path = socket_path("hello");
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
        let path = socket_path("closed");
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
        let path = socket_path("call");
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
    fn wait_event_serves_an_event_then_reports_the_close() {
        let path = socket_path("wait");
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
        let path = socket_path("mismatch");
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
