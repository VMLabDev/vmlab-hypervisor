//! Reconnect display transport, including replacement processes at the same socket.

use std::io;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vmlab_hv_display::{Client, Message, Packet};

pub(crate) enum Event {
    Connected(u64, Arc<Client>),
    Packet(u64, Packet),
    Disconnected(u64, io::Error),
    Gone,
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
    stream: Arc<Mutex<UnixStream>>,
    thread: Option<JoinHandle<()>>,
}

impl Connection {
    pub(crate) fn start(
        path: PathBuf,
        initial: Arc<Client>,
        mut notify: impl FnMut(Event) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let mut peer = Peer::new(initial.stream())?;
        initial
            .stream()
            .set_write_timeout(Some(Duration::from_secs(2)))?;
        let stream = Arc::new(Mutex::new(initial.stream().try_clone()?));
        let current = stream.clone();
        let (stop, cancelled) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("viewer-display".into())
            .spawn(move || {
                let capabilities = initial.capabilities;
                let mut source = initial;
                let mut generation = 0;
                loop {
                    let error = loop {
                        if cancelled.try_recv().is_ok() {
                            return;
                        }
                        let mut descriptors = [
                            libc::pollfd {
                                fd: source.stream().as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: peer.fd.as_raw_fd(),
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
                        let packet = source.receive().and_then(|packet| {
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
                    let _ = source.stream().shutdown(Shutdown::Both);
                    if !notify(Event::Disconnected(generation, error)) {
                        return;
                    }
                    let mut delay = Duration::from_millis(100);
                    let mut gone_at = None;
                    loop {
                        if !peer.alive() {
                            let since = gone_at.get_or_insert_with(Instant::now);
                            if since.elapsed() >= Duration::from_secs(5) {
                                notify(Event::Gone);
                                return;
                            }
                        }
                        if cancelled.recv_timeout(delay) != Err(mpsc::RecvTimeoutError::Timeout) {
                            return;
                        }
                        delay = (delay * 2).min(Duration::from_secs(2));
                        let attempt = (|| -> io::Result<(Client, Peer)> {
                            let stream = UnixStream::connect(&path)?;
                            let replacement = Peer::new(&stream)?;
                            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                            *current.lock().unwrap() = stream.try_clone()?;
                            let client = Client::from_stream(stream, capabilities)?;
                            if !client
                                .capabilities
                                .has(vmlab_hv_display::Capabilities::FRAMES)
                            {
                                return Err(crate::invalid("server did not negotiate frames"));
                            }
                            client.stream().set_read_timeout(None)?;
                            Ok((client, replacement))
                        })();
                        if let Ok((client, replacement)) = attempt {
                            if cancelled.try_recv().is_ok() {
                                return;
                            }
                            source = Arc::new(client);
                            peer = replacement;
                            generation += 1;
                            if !notify(Event::Connected(generation, source.clone())) {
                                return;
                            }
                            break;
                        }
                    }
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
        let _ = self.stream.lock().unwrap().shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
    fn truncated_packet_reconnects_and_drop_cancels_a_stalled_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display.sock");
        let (client, mut server) = initial();
        let (sender, events) = mpsc::channel();
        let connection = Connection::start(path.clone(), client, move |event| {
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
        let connection = Connection::start(path.clone(), client, move |event| {
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
            Connection::start(path, client, move |event| sender.send(event).is_ok()).unwrap();
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
