//! Reconnect display transport, including replacement processes at the same socket.

use std::io;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vmlab_hv_display::{Capabilities, Client, Message, Packet};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

fn connect(path: &Path) -> io::Result<UnixStream> {
    // SAFETY: zero is valid for sockaddr_un; the family and path are set below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid display socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, byte) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *byte as libc::c_char;
    }
    // SAFETY: socket takes constants and returns a new owned descriptor.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful socket call returned a new descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: address is initialized and its pointer and length match sockaddr_un.
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::EINPROGRESS) {
            io::ErrorKind::WouldBlock.into()
        } else {
            error
        });
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

pub(crate) enum Event {
    Connected(u64, Arc<Client>),
    Packet(u64, Packet),
    Disconnected(u64, io::Error),
    Gone,
    Failed(io::Error),
}

struct Peer {
    fd: OwnedFd,
}

fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    // SAFETY: both pointers refer to correctly sized, writable objects.
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.pid)
    }
}

impl Peer {
    fn new(stream: &UnixStream) -> io::Result<Self> {
        let pid = peer_pid(stream)?;
        // SAFETY: pidfd_open takes a PID and flags, and returns an owned descriptor.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: the successful syscall returned a new descriptor.
            fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
        })
    }

    fn alive(&self) -> bool {
        let mut poll = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points at one live descriptor and does not retain the pointer.
        unsafe { libc::poll(&mut poll, 1, 0) <= 0 }
    }
}

pub(crate) struct Connection {
    stop: mpsc::Sender<()>,
    stream: Arc<Mutex<Option<UnixStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl Connection {
    pub(crate) fn start(
        path: PathBuf,
        shm: bool,
        notify: impl FnMut(Event) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn(
            path,
            None,
            crate::Core::capabilities(shm),
            STARTUP_TIMEOUT,
            notify,
        )
    }

    #[cfg(test)]
    fn from_client(
        path: PathBuf,
        initial: Arc<Client>,
        notify: impl FnMut(Event) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let capabilities = initial.capabilities;
        Self::spawn(path, Some(initial), capabilities, STARTUP_TIMEOUT, notify)
    }

    fn spawn(
        path: PathBuf,
        initial: Option<Arc<Client>>,
        capabilities: Capabilities,
        startup_timeout: Duration,
        mut notify: impl FnMut(Event) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let mut peer = initial
            .as_ref()
            .map(|client| Peer::new(client.stream()))
            .transpose()?;
        let initial_stream = initial
            .as_ref()
            .map(|client| {
                client
                    .stream()
                    .set_write_timeout(Some(Duration::from_secs(2)))?;
                client.stream().try_clone()
            })
            .transpose()?;
        let stream = Arc::new(Mutex::new(initial_stream));
        let current = stream.clone();
        let (stop, cancelled) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("viewer-display".into())
            .spawn(move || {
                let mut source = initial;
                let mut generation = 0;
                let startup_deadline = Instant::now() + startup_timeout;
                let mut delay = Duration::ZERO;
                let mut gone_at = None;
                loop {
                    if source.is_none() {
                        let startup = peer.is_none();
                        let deadline = if startup {
                            Some(startup_deadline)
                        } else if peer.as_ref().is_some_and(|peer| !peer.alive()) {
                            Some(*gone_at.get_or_insert_with(Instant::now) + Duration::from_secs(5))
                        } else {
                            None
                        };
                        let remaining = deadline.map_or(Duration::MAX, |until| {
                            until.saturating_duration_since(Instant::now())
                        });
                        if remaining.is_zero() {
                            if startup {
                                notify(Event::Failed(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "timed out waiting for the display socket",
                                )));
                            } else {
                                notify(Event::Gone);
                            }
                            return;
                        }
                        if cancelled.recv_timeout(delay.min(remaining))
                            != Err(mpsc::RecvTimeoutError::Timeout)
                        {
                            return;
                        }
                        let remaining = deadline.map_or(Duration::from_secs(2), |until| {
                            until
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_secs(2))
                        });
                        if remaining.is_zero() {
                            continue;
                        }
                        let attempt = (|| -> io::Result<(Arc<Client>, Peer)> {
                            let stream = connect(&path)?;
                            let replacement = Peer::new(&stream)?;
                            stream.set_read_timeout(Some(remaining))?;
                            stream.set_write_timeout(Some(remaining))?;
                            *current.lock().unwrap() = Some(stream.try_clone()?);
                            let client = Client::from_stream(stream, capabilities)?;
                            if !client.capabilities.has(Capabilities::FRAMES) {
                                return Err(crate::invalid("server did not negotiate frames"));
                            }
                            client.stream().set_read_timeout(None)?;
                            client
                                .stream()
                                .set_write_timeout(Some(Duration::from_secs(2)))?;
                            Ok((Arc::new(client), replacement))
                        })();
                        match attempt {
                            Ok((client, replacement)) => {
                                if cancelled.try_recv().is_ok() {
                                    return;
                                }
                                source = Some(client.clone());
                                peer = Some(replacement);
                                if !notify(Event::Connected(generation, client)) {
                                    return;
                                }
                            }
                            Err(error) => {
                                current.lock().unwrap().take();
                                if startup && !retryable(&error) {
                                    notify(Event::Failed(error));
                                    return;
                                }
                                delay = (delay.max(Duration::from_millis(50)) * 2)
                                    .min(Duration::from_secs(2));
                                continue;
                            }
                        }
                    }
                    let active = source.take().unwrap();
                    let error = loop {
                        if cancelled.try_recv().is_ok() {
                            return;
                        }
                        let mut descriptors = [
                            libc::pollfd {
                                fd: active.stream().as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: peer.as_ref().unwrap().fd.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        // SAFETY: both descriptors remain owned for the entire poll.
                        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
                        if ready < 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            break error;
                        }
                        if descriptors[1].revents != 0 {
                            break io::Error::new(
                                io::ErrorKind::ConnectionReset,
                                "display process exited",
                            );
                        }
                        let packet = active.receive().and_then(|packet| {
                            if matches!(packet.message, Message::FrameReady { .. }) {
                                for fd in &packet.fds {
                                    crate::wait_fence(fd.as_raw_fd())?;
                                }
                            }
                            Ok(packet)
                        });
                        match packet {
                            Ok(packet) => {
                                if !notify(Event::Packet(generation, packet)) {
                                    return;
                                }
                            }
                            Err(error) => break error,
                        }
                    };
                    let _ = active.stream().shutdown(Shutdown::Both);
                    if !notify(Event::Disconnected(generation, error)) {
                        return;
                    }
                    generation += 1;
                    delay = Duration::from_millis(100);
                    gone_at = None;
                }
            })?;
        Ok(Self {
            stop,
            stream,
            thread: Some(thread),
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(stream) = self.stream.lock().unwrap().as_ref() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;
    use vmlab_hv_display::{Capabilities, receive, send};

    fn handshake(stream: &UnixStream) {
        let packet = receive(stream).unwrap();
        assert!(matches!(packet.message, Message::Hello { .. }));
        send(stream, &packet.message, &[]).unwrap();
    }

    fn initial() -> (Arc<Client>, UnixStream) {
        let (server, client) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            handshake(&server);
            server
        });
        let client = Arc::new(Client::from_stream(client, Capabilities::ALL).unwrap());
        (client, thread.join().unwrap())
    }

    #[test]
    fn startup_deadline_bounds_missing_refused_and_stalled_sockets() {
        assert_eq!(STARTUP_TIMEOUT, Duration::from_secs(120));
        for mode in ["missing", "refused", "stalled"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("display.sock");
            let listener = (mode != "missing").then(|| UnixListener::bind(&path).unwrap());
            let listener = if mode == "refused" {
                drop(listener);
                None
            } else {
                listener
            };
            let (sender, events) = mpsc::channel();
            let started = Instant::now();
            let connection = Connection::spawn(
                path,
                None,
                Capabilities::ALL,
                Duration::from_millis(150),
                move |event| sender.send(event).is_ok(),
            )
            .unwrap();
            let _stalled = listener.map(|listener| {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                assert!(matches!(
                    receive(&stream).unwrap().message,
                    Message::Hello { .. }
                ));
                stream
            });
            assert!(
                matches!(events.recv_timeout(Duration::from_secs(3)).unwrap(), Event::Failed(error) if error.kind() == io::ErrorKind::TimedOut)
            );
            assert!(started.elapsed() >= Duration::from_millis(150));
            drop(connection);
            assert!(matches!(
                events.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ));
        }
    }

    #[test]
    fn full_listener_backlog_cannot_block_the_startup_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // SAFETY: this test owns the listener; a zero backlog bounds the fixture.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _pending = connect(&path).unwrap();
        assert_eq!(
            connect(&path).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let (sender, events) = mpsc::channel();
        let _connection = Connection::spawn(
            path,
            None,
            Capabilities::ALL,
            Duration::from_millis(150),
            move |event| sender.send(event).is_ok(),
        )
        .unwrap();
        assert!(
            matches!(events.recv_timeout(Duration::from_secs(3)).unwrap(), Event::Failed(error) if error.kind() == io::ErrorKind::TimedOut)
        );
    }

    #[test]
    fn startup_wait_can_be_cancelled_without_a_peer() {
        let directory = tempfile::tempdir().unwrap();
        let (sender, events) = mpsc::channel();
        let connection =
            Connection::start(directory.path().join("missing.sock"), true, move |event| {
                sender.send(event).is_ok()
            })
            .unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let started = Instant::now();
        drop(connection);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn truncated_packet_reconnects_and_drop_cancels_a_stalled_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display.sock");
        let (client, mut server) = initial();
        let (sender, events) = mpsc::channel();
        let connection = Connection::from_client(path.clone(), client, move |event| {
            sender.send(event).is_ok()
        })
        .unwrap();
        server.write_all(&[1, 0]).unwrap();
        drop(server);
        assert!(
            matches!(events.recv_timeout(Duration::from_secs(3)).unwrap(), Event::Disconnected(0, error) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
        let listener = UnixListener::bind(&path).unwrap();
        let (resumed, _) = listener.accept().unwrap();
        handshake(&resumed);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Connected(1, _)
        ));
        send(&resumed, &Message::LedState { mask: 4 }, &[]).unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Packet(
                1,
                Packet {
                    message: Message::LedState { mask: 4 },
                    ..
                }
            )
        ));
        drop(resumed);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Disconnected(1, _)
        ));
        let (stalled, _) = listener.accept().unwrap();
        assert!(matches!(
            receive(&stalled).unwrap().message,
            Message::Hello { .. }
        ));
        let start = Instant::now();
        drop(connection);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn peer_helper() {
        let Some(path) = std::env::var_os("VMLAB_VIEWER_PEER_HELPER") else {
            return;
        };
        let listener = UnixListener::bind(path).unwrap();
        let (stream, _) = listener.accept().unwrap();
        handshake(&stream);
        let _ = receive(&stream);
    }

    #[test]
    fn replacement_process_at_the_same_socket_gets_a_fresh_connection() {
        fn spawn(path: &std::path::Path) -> vmlab_hv_tests::process::ManagedChild {
            let child = vmlab_hv_tests::process::ManagedChild::helper(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "connection::tests::peer_helper"])
                    .env("VMLAB_VIEWER_PEER_HELPER", path)
                    .stdout(std::process::Stdio::null()),
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !path.exists() {
                assert!(Instant::now() < deadline);
                std::thread::park_timeout(Duration::from_millis(10));
            }
            child
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display.sock");
        let mut old = spawn(&path);
        let client = Arc::new(Client::connect(&path, Capabilities::ALL).unwrap());
        let (sender, events) = mpsc::channel();
        let connection = Connection::from_client(path.clone(), client, move |event| {
            sender.send(event).is_ok()
        })
        .unwrap();
        old.kill().unwrap();
        old.wait().unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut replacement = spawn(&path);
        loop {
            match events.recv_timeout(Duration::from_secs(3)).unwrap() {
                Event::Disconnected(0, _) => {}
                Event::Connected(1, client) => {
                    assert_eq!(
                        peer_pid(client.stream()).unwrap(),
                        replacement.id() as libc::pid_t
                    );
                    break;
                }
                _ => panic!("replacement process was not reconnected"),
            }
        }
        drop(connection);
        assert!(replacement.wait().unwrap().success());
    }

    #[test]
    fn original_process_exit_is_distinct_from_a_display_disconnect() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display.sock");
        let mut child = vmlab_hv_tests::process::ManagedChild::helper(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "connection::tests::peer_helper"])
                .env("VMLAB_VIEWER_PEER_HELPER", &path)
                .stdout(std::process::Stdio::null()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !path.exists() {
            assert!(Instant::now() < deadline);
            std::thread::park_timeout(Duration::from_millis(10));
        }
        let client = Arc::new(Client::connect(&path, Capabilities::ALL).unwrap());
        let (sender, events) = mpsc::channel();
        let _connection =
            Connection::from_client(path, client, move |event| sender.send(event).is_ok()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        loop {
            match events.recv_timeout(Duration::from_secs(8)).unwrap() {
                Event::Gone => break,
                Event::Disconnected(..) => {}
                _ => panic!("unexpected reconnect to a dead machine"),
            }
        }
    }
}
