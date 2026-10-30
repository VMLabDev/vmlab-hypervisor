//! The server side of the control protocol.
//!
//! [`Server`] serves connections from a [`UnixListener`], one thread each,
//! handing every post-handshake [`Command`] to a caller-supplied
//! `FnMut(Command) -> Outcome` and pushing [`Event`]s from anywhere through
//! [`Server::broadcast`]. Everything is synchronous std code: the VMM runs
//! `serve` on its own thread.
//!
//! Per connection, the state machine is:
//!
//! * before a handshake, every [`ClientMsg::Request`] is answered with
//!   [`ErrorCode::NoHandshake`] and the connection stays open;
//! * a [`ClientMsg::Hello`] with a different version is answered with an
//!   id-less [`ServerMsg::Error`] carrying [`ErrorCode::VersionMismatch`]
//!   and the connection is closed;
//! * after the handshake, requests go through the handler and the outcome is
//!   replied with the request id. A repeated `Hello` with the agreed version
//!   is answered with the same `Hello` again: it needs no error code and a
//!   client that retries the handshake is not a threat.

use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;

use crate::PROTO_VERSION;
use crate::codec::{self, CodecError};
use crate::control::{Capability, ClientMsg, Command, ErrorCode, Event, Outcome, ServerMsg};

/// Serves connections with one shared handler and broadcasts events to all of
/// them. Clone it to keep a handle for [`Server::broadcast`] while another
/// thread runs [`Server::serve`].
pub struct Server<H> {
    shared: Arc<Shared<H>>,
}

impl<H> Clone for Server<H> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

struct Shared<H> {
    closed: AtomicBool,
    operation: Mutex<()>,
    hv_version: String,
    capabilities: Vec<Capability>,
    handler: Mutex<H>,
    /// One write half per live connection; entries are pruned when the
    /// connection that owns them has gone away.
    writers: Mutex<Vec<Weak<Mutex<UnixStream>>>>,
}

impl<H: FnMut(Command) -> Outcome + Send + 'static> Server<H> {
    /// Build a server that introduces itself as `hv_version` advertising
    /// `capabilities` and answers commands with `handler`.
    pub fn new(hv_version: impl Into<String>, capabilities: Vec<Capability>, handler: H) -> Self {
        Self {
            shared: Arc::new(Shared {
                closed: AtomicBool::new(false),
                operation: Mutex::new(()),
                hv_version: hv_version.into(),
                capabilities,
                handler: Mutex::new(handler),
                writers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Accept connections from `listener` and serve each on its own thread.
    /// Blocks the calling thread for the lifetime of the listener; failures
    /// of a single connection end only that connection.
    pub fn serve(&self, listener: UnixListener) -> io::Result<()> {
        self.serve_until(listener, || false)
    }

    /// Stop accepting and close every connection when the owner has exited.
    pub fn serve_until(
        &self,
        listener: UnixListener,
        stopped: impl Fn() -> bool,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        let mut workers = Vec::new();
        while !stopped() && !self.shared.closed.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let shared = Arc::clone(&self.shared);
            workers.retain(|worker: &thread::JoinHandle<()>| !worker.is_finished());
            workers.push(
                thread::Builder::new()
                    .name("control-connection".to_owned())
                    .spawn(move || {
                        // Errors end this connection; the listener serves on.
                        let _ = serve_connection(&shared, stream);
                    })?,
            );
        }
        self.shutdown();
        for worker in workers {
            let _ = worker.join();
        }
        Ok(())
    }
}

impl<H> Server<H> {
    /// Interrupt idle readers and blocked writers without waiting for a handshake.
    pub fn shutdown(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        // Let an in-flight Quit send its reply before closing the socket.
        let _operation = lock(&self.shared.operation);
        for writer in lock(&self.shared.writers)
            .drain(..)
            .filter_map(|w| w.upgrade())
        {
            let _ = lock(&writer).shutdown(std::net::Shutdown::Both);
        }
    }
    /// Send `event` to every connected client, pruning connections that have
    /// gone away. Returns once the event has been written (or failed) to
    /// each of them.
    pub fn broadcast(&self, event: Event) {
        let mut writers = lock(&self.shared.writers);
        writers.retain(|weak| match weak.upgrade() {
            // The connection is gone; forget it.
            None => false,
            Some(writer) => write_to(
                &writer,
                &ServerMsg::Event {
                    event: event.clone(),
                },
            )
            .is_ok(),
        });
    }
}

fn serve_connection<H: FnMut(Command) -> Outcome>(
    shared: &Arc<Shared<H>>,
    stream: UnixStream,
) -> Result<(), ServeError> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    stream.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
    {
        let mut writers = lock(&shared.writers);
        writers.retain(|writer| writer.strong_count() != 0);
        writers.push(Arc::downgrade(&writer));
        if shared.closed.load(Ordering::SeqCst) {
            stream.shutdown(std::net::Shutdown::Both)?;
            return Ok(());
        }
    }
    let mut reader = BufReader::new(stream);
    let mut phase = Phase::AwaitingHello;
    // EOF means the peer closed; nothing to answer.
    while let Some(msg) = codec::read_client_msg(&mut reader)? {
        let _operation = lock(&shared.operation);
        if shared.closed.load(Ordering::SeqCst) {
            break;
        }
        match respond(shared, &mut phase, msg) {
            Response::Send(msg) => write_to(&writer, &msg)?,
            Response::SendThenClose(msg) => {
                write_to(&writer, &msg)?;
                break;
            }
        }
    }
    Ok(())
}

/// What one connection is waiting for.
#[derive(Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for a versioned `Hello`.
    AwaitingHello,
    /// Handshake complete; requests are served.
    Established,
}

/// What the connection should do after handling one message.
#[derive(Debug, PartialEq)]
enum Response {
    /// Send this message and keep serving.
    Send(ServerMsg),
    /// Send this message, then close.
    SendThenClose(ServerMsg),
}

fn respond<H: FnMut(Command) -> Outcome>(
    shared: &Shared<H>,
    phase: &mut Phase,
    msg: ClientMsg,
) -> Response {
    match msg {
        ClientMsg::Hello { proto_version } => {
            if proto_version == PROTO_VERSION {
                *phase = Phase::Established;
                Response::Send(ServerMsg::Hello {
                    proto_version: PROTO_VERSION,
                    hv_version: shared.hv_version.clone(),
                    capabilities: shared.capabilities.clone(),
                })
            } else {
                Response::SendThenClose(ServerMsg::Error {
                    code: ErrorCode::VersionMismatch,
                    message: format!(
                        "client protocol version {proto_version}, server speaks {PROTO_VERSION}"
                    ),
                })
            }
        }
        ClientMsg::Request { id, command } => {
            if *phase == Phase::AwaitingHello {
                return Response::Send(error_reply(
                    id,
                    ErrorCode::NoHandshake,
                    "handshake required before requests",
                ));
            }
            let mut handler = lock(&shared.handler);
            let outcome = (*handler)(command);
            Response::Send(ServerMsg::Reply { id, outcome })
        }
    }
}

fn error_reply(id: u64, code: ErrorCode, message: impl Into<String>) -> ServerMsg {
    ServerMsg::Reply {
        id,
        outcome: Outcome::Error {
            code,
            message: message.into(),
        },
    }
}

fn write_to(writer: &Mutex<UnixStream>, msg: &ServerMsg) -> Result<(), ServeError> {
    let mut writer = lock(writer);
    codec::write_server_msg(&mut *writer, msg)?;
    Ok(())
}

/// Lock a mutex, keeping the state of a handler that panicked mid-command: a
/// panic ends that command, not every later one.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, thiserror::Error)]
enum ServeError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Codec(#[from] CodecError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{RunState, ShutdownReason};
    use std::path::PathBuf;

    fn test_shared() -> Shared<impl FnMut(Command) -> Outcome> {
        Shared {
            closed: AtomicBool::new(false),
            operation: Mutex::new(()),
            hv_version: "test-hv".to_owned(),
            capabilities: vec![Capability::Snapshots],
            handler: Mutex::new(|_| Outcome::Ok {
                status: Some(RunState::Running),
            }),
            writers: Mutex::new(Vec::new()),
        }
    }

    #[test]
    fn connection_registry_is_bounded_without_events() {
        let shared = Arc::new(test_shared());
        for _ in 0..1000 {
            let (server, client) = UnixStream::pair().unwrap();
            drop(client);
            serve_connection(&shared, server).unwrap();
        }
        let retained = lock(&shared.writers).len();
        assert!(
            retained <= 1,
            "registry retained {retained} dead connections"
        );
    }

    #[test]
    fn shutdown_interrupts_a_client_without_a_handshake() {
        let (_directory, path) = socket_path("shutdown");
        let server = Server::new("test", vec![], |_| Outcome::Ok { status: None });
        let owner = server.clone();
        let listener = UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || server.serve(listener).unwrap());
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        owner.shutdown();
        use std::io::Read;
        match client.read(&mut [0]) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            result => panic!("shutdown did not close the socket: {result:?}"),
        }
        worker.join().unwrap();
    }

    #[test]
    fn request_before_handshake_gets_no_handshake() {
        let shared = test_shared();
        let mut phase = Phase::AwaitingHello;
        let response = respond(
            &shared,
            &mut phase,
            ClientMsg::Request {
                id: 5,
                command: Command::Status,
            },
        );
        assert_eq!(
            response,
            Response::Send(error_reply(
                5,
                ErrorCode::NoHandshake,
                "handshake required before requests"
            ))
        );
        // The connection stays in the pre-handshake phase.
        assert_eq!(phase, Phase::AwaitingHello);
    }

    #[test]
    fn matching_hello_completes_the_handshake() {
        let shared = test_shared();
        let mut phase = Phase::AwaitingHello;
        let response = respond(
            &shared,
            &mut phase,
            ClientMsg::Hello {
                proto_version: PROTO_VERSION,
            },
        );
        assert_eq!(
            response,
            Response::Send(ServerMsg::Hello {
                proto_version: PROTO_VERSION,
                hv_version: "test-hv".to_owned(),
                capabilities: vec![Capability::Snapshots],
            })
        );
        assert_eq!(phase, Phase::Established);
    }

    #[test]
    fn wrong_version_is_rejected_and_closes() {
        let shared = test_shared();
        let mut phase = Phase::AwaitingHello;
        let response = respond(
            &shared,
            &mut phase,
            ClientMsg::Hello {
                proto_version: PROTO_VERSION + 1,
            },
        );
        match response {
            Response::SendThenClose(ServerMsg::Error { code, message }) => {
                assert_eq!(code, ErrorCode::VersionMismatch);
                assert!(
                    message.contains("server speaks 1"),
                    "message names both versions: {message}"
                );
            }
            other => panic!("expected close-after-error, got {other:?}"),
        }
    }

    #[test]
    fn request_after_handshake_reaches_the_handler() {
        let shared = test_shared();
        let mut phase = Phase::Established;
        let response = respond(
            &shared,
            &mut phase,
            ClientMsg::Request {
                id: 9,
                command: Command::SnapshotSave {
                    name: "snap".to_owned(),
                },
            },
        );
        assert_eq!(
            response,
            Response::Send(ServerMsg::Reply {
                id: 9,
                outcome: Outcome::Ok {
                    status: Some(RunState::Running),
                },
            })
        );
    }

    #[test]
    fn repeated_matching_hello_is_idempotent() {
        let shared = test_shared();
        let mut phase = Phase::Established;
        let response = respond(
            &shared,
            &mut phase,
            ClientMsg::Hello {
                proto_version: PROTO_VERSION,
            },
        );
        assert!(matches!(response, Response::Send(ServerMsg::Hello { .. })));
        assert_eq!(phase, Phase::Established);
    }

    /// A raw protocol client with a persistent read buffer, for driving the
    /// server without the client module.
    struct RawClient {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl RawClient {
        fn connect(path: &std::path::Path) -> Self {
            let stream = UnixStream::connect(path).unwrap();
            let mut raw = Self {
                writer: stream.try_clone().unwrap(),
                reader: BufReader::new(stream),
            };
            raw.hello();
            raw
        }

        fn hello(&mut self) -> ServerMsg {
            codec::write_client_msg(
                &mut self.writer,
                &ClientMsg::Hello {
                    proto_version: PROTO_VERSION,
                },
            )
            .unwrap();
            self.recv().unwrap()
        }

        fn recv(&mut self) -> Option<ServerMsg> {
            codec::read_server_msg(&mut self.reader).unwrap()
        }
    }

    fn socket_path(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("hv-server-{name}-"))
            .tempdir()
            .unwrap();
        let path = dir.path().join("control.sock");
        (dir, path)
    }

    fn spawn_server(
        server: Server<impl FnMut(Command) -> Outcome + Send + 'static>,
        path: &std::path::Path,
    ) {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || server.serve(listener).unwrap());
    }

    #[test]
    fn requests_then_handshake_then_request_over_a_socket() {
        let (_dir, path) = socket_path("no-handshake");
        let server = Server::new("test-hv", vec![Capability::Snapshots], |command| {
            assert_eq!(command, Command::Status);
            Outcome::Ok {
                status: Some(RunState::Paused),
            }
        });
        spawn_server(server, &path);

        let stream = UnixStream::connect(&path).unwrap();
        let mut raw = RawClient {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        };
        // A request before the handshake is refused but the connection lives.
        codec::write_client_msg(
            &mut raw.writer,
            &ClientMsg::Request {
                id: 3,
                command: Command::Status,
            },
        )
        .unwrap();
        assert_eq!(
            raw.recv(),
            Some(error_reply(
                3,
                ErrorCode::NoHandshake,
                "handshake required before requests"
            ))
        );
        // The handshake then succeeds and the same request is served.
        assert!(matches!(raw.hello(), ServerMsg::Hello { .. }));
        codec::write_client_msg(
            &mut raw.writer,
            &ClientMsg::Request {
                id: 4,
                command: Command::Status,
            },
        )
        .unwrap();
        assert_eq!(
            raw.recv(),
            Some(ServerMsg::Reply {
                id: 4,
                outcome: Outcome::Ok {
                    status: Some(RunState::Paused),
                },
            })
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wrong_version_over_a_socket_gets_error_then_close() {
        let (_dir, path) = socket_path("version");
        let server = Server::new("test-hv", vec![], |_| Outcome::Ok { status: None });
        spawn_server(server, &path);

        let stream = UnixStream::connect(&path).unwrap();
        let mut raw = RawClient {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        };
        codec::write_client_msg(&mut raw.writer, &ClientMsg::Hello { proto_version: 99 }).unwrap();
        match raw.recv() {
            Some(ServerMsg::Error { code, .. }) => assert_eq!(code, ErrorCode::VersionMismatch),
            other => panic!("expected version mismatch, got {other:?}"),
        }
        // The server closes after the rejection.
        assert_eq!(raw.recv(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn broadcast_lands_between_request_and_reply() {
        let (_dir, path) = socket_path("mid-event");
        let (saw_request, request_seen) = std::sync::mpsc::channel::<()>();
        let (event_sent, send_event) = std::sync::mpsc::channel::<()>();
        let server = Server::new("test-hv", vec![], move |command| {
            if command == Command::Powerdown {
                saw_request.send(()).unwrap();
                // Hold the reply until the event is on the wire.
                send_event.recv().unwrap();
            }
            Outcome::Ok { status: None }
        });
        let broadcaster = server.clone();
        spawn_server(server, &path);
        thread::spawn(move || {
            request_seen.recv().unwrap();
            broadcaster.broadcast(Event::Shutdown {
                reason: ShutdownReason::GuestShutdown,
            });
            event_sent.send(()).unwrap();
        });

        let mut raw = RawClient::connect(&path);
        codec::write_client_msg(
            &mut raw.writer,
            &ClientMsg::Request {
                id: 1,
                command: Command::Powerdown,
            },
        )
        .unwrap();
        // The event was pushed while the request was being served, so it
        // arrives before the reply.
        assert_eq!(
            raw.recv(),
            Some(ServerMsg::Event {
                event: Event::Shutdown {
                    reason: ShutdownReason::GuestShutdown,
                },
            })
        );
        assert_eq!(
            raw.recv(),
            Some(ServerMsg::Reply {
                id: 1,
                outcome: Outcome::Ok { status: None },
            })
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn broadcast_after_disconnect_is_dropped() {
        let (_dir, path) = socket_path("prune");
        let server = Server::new("test-hv", vec![], |_| Outcome::Ok { status: None });
        let broadcaster = server.clone();
        spawn_server(server, &path);

        let raw = RawClient::connect(&path);
        drop(raw);
        // Give the connection thread time to see the EOF, then broadcast to
        // the vanished client: it must neither hang nor panic.
        thread::sleep(std::time::Duration::from_millis(50));
        broadcaster.broadcast(Event::Reset);
        let _ = std::fs::remove_file(&path);
    }
}
