//! Request-boundary SCM_RIGHTS transport for the Unix control socket.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

use crate::codec::{CodecError, MAX_LINE_BYTES};
use crate::control::{ClientMsg, Command};

/// Up to 31 TAP queues followed by 31 matching vhost-net files.
pub(crate) const MAX_FDS: usize = 62;

pub(crate) fn validate_count(command: &Command, count: usize) -> io::Result<()> {
    let expected = match command {
        Command::NetAttach {
            queue_pairs, vhost, ..
        } => {
            if !(1..=31).contains(queue_pairs) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "net_attach needs 1..31 queue pairs",
                ));
            }
            usize::from(*queue_pairs) * if *vhost { 2 } else { 1 }
        }
        _ => 0,
    };
    if count != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("request needs {expected} descriptors, got {count}"),
        ));
    }
    Ok(())
}

pub(crate) fn write_client_msg(
    stream: &UnixStream,
    message: &ClientMsg,
    fds: &[BorrowedFd<'_>],
) -> Result<(), CodecError> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many descriptors").into());
    }
    let mut bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_LINE_BYTES {
        return Err(CodecError::LineTooLong);
    }
    bytes.push(b'\n');
    send_all(&bytes, fds, |bytes, fds| send_chunk(stream, bytes, fds))?;
    Ok(())
}

fn send_all(
    mut bytes: &[u8],
    mut fds: &[BorrowedFd<'_>],
    mut send: impl FnMut(&[u8], &[BorrowedFd<'_>]) -> io::Result<usize>,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match send(bytes, fds) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "control socket closed",
                ));
            }
            Ok(count) => {
                bytes = &bytes[count..];
                // A successful send transfers rights once, even after a short write.
                fds = &[];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn send_chunk(stream: &UnixStream, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !ancillary.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors",
        ));
    }
    sendmsg(
        stream,
        &[IoSlice::new(bytes)],
        &mut ancillary,
        SendFlags::NOSIGNAL,
    )
    .map_err(io::Error::from)
}

pub(crate) fn read_client_msg(
    stream: &UnixStream,
) -> Result<Option<(ClientMsg, Vec<OwnedFd>)>, CodecError> {
    let mut bytes = Vec::new();
    let mut fds = Vec::new();
    loop {
        // Reading exactly one byte prevents a request from consuming rights
        // attached to any byte of the following pipelined request.
        let mut byte = [0];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let received = match recvmsg(
            stream,
            &mut [IoSliceMut::new(&mut byte)],
            &mut ancillary,
            RecvFlags::CMSG_CLOEXEC,
        ) {
            Ok(received) => received,
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(io::Error::from(error).into()),
        };
        if received.flags.contains(ReturnFlags::CTRUNC) {
            // Dropping rustix's ancillary buffer closes every received right.
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated descriptors").into());
        }
        for message in ancillary.drain() {
            match message {
                RecvAncillaryMessage::ScmRights(rights) => fds.extend(rights),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected ancillary message",
                    )
                    .into());
                }
            }
        }
        if fds.len() > MAX_FDS {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "too many descriptors").into());
        }
        if received.bytes == 0 {
            if bytes.is_empty() && fds.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() == MAX_LINE_BYTES {
            return Err(CodecError::LineTooLong);
        }
        bytes.push(byte[0]);
    }
    Ok(Some((serde_json::from_slice(&bytes)?, fds)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixDatagram;
    use std::time::Duration;

    fn attach(id: u64, queue_pairs: u16, vhost: bool) -> ClientMsg {
        ClientMsg::Request {
            id,
            command: Command::NetAttach {
                mac: "52:54:00:12:34:56".into(),
                queue_pairs,
                vhost,
            },
        }
    }

    fn raw_line(message: &ClientMsg) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(message).unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn pair() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (a, b)
    }

    #[test]
    fn count_contract_accepts_only_matching_tap_and_vhost_vectors() {
        for pairs in 1..=31 {
            for vhost in [false, true] {
                let ClientMsg::Request { command, .. } = attach(1, pairs, vhost) else {
                    unreachable!()
                };
                let count = usize::from(pairs) * if vhost { 2 } else { 1 };
                assert!(validate_count(&command, count).is_ok());
                assert!(validate_count(&command, count - 1).is_err());
                assert!(validate_count(&command, count + 1).is_err());
            }
        }
        for pairs in [0, 32, u16::MAX] {
            let ClientMsg::Request { command, .. } = attach(1, pairs, true) else {
                unreachable!()
            };
            assert!(validate_count(&command, 0).is_err());
        }
        assert!(validate_count(&Command::Status, 0).is_ok());
        assert!(validate_count(&Command::Status, 1).is_err());
    }

    #[test]
    fn short_writes_and_interrupts_transfer_rights_exactly_once() {
        let (sender, receiver) = pair();
        let (original, mut peer) = UnixStream::pair().unwrap();
        let message = attach(7, 1, false);
        let bytes = raw_line(&message);
        let mut calls = 0;
        send_all(&bytes, &[original.as_fd()], |bytes, fds| {
            calls += 1;
            if matches!(calls, 1 | 3) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            send_chunk(&sender, &bytes[..bytes.len().min(7)], fds)
        })
        .unwrap();
        assert!(calls > 3);
        let (actual, mut received) = read_client_msg(&receiver).unwrap().unwrap();
        assert_eq!(actual, message);
        assert_eq!(received.len(), 1);
        let fd = received.pop().unwrap();
        assert!(
            rustix::io::fcntl_getfd(&fd)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        let mut passed = UnixStream::from(fd);
        passed.write_all(b"passed").unwrap();
        let mut bytes = [0; 6];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"passed");
        let mut caller = &original;
        caller.write_all(b"caller").unwrap();
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"caller");
    }

    #[test]
    fn fragmented_and_pipelined_requests_keep_their_own_rights() {
        let (mut sender, receiver) = pair();
        let first = ClientMsg::Request {
            id: 1,
            command: Command::Status,
        };
        let second = attach(2, 2, true);
        let third = ClientMsg::Request {
            id: 3,
            command: Command::Pause,
        };
        let rights: Vec<_> = (0..4).map(|_| UnixDatagram::unbound().unwrap()).collect();
        let borrowed: Vec<_> = rights.iter().map(AsFd::as_fd).collect();
        let bytes = raw_line(&first);
        sender.write_all(&bytes[..bytes.len() - 1]).unwrap();
        // Rights are attached exactly to the start of request two, after the
        // newline of request one. A buffered recvmsg would mix the requests.
        sender.write_all(b"\n").unwrap();
        let bytes = raw_line(&second);
        send_chunk(&sender, &bytes[..9], &borrowed[..2]).unwrap();
        send_chunk(&sender, &bytes[9..17], &borrowed[2..]).unwrap();
        sender.write_all(&bytes[17..]).unwrap();
        sender.write_all(&raw_line(&third)).unwrap();
        let (message, fds) = read_client_msg(&receiver).unwrap().unwrap();
        assert_eq!(message, first);
        assert!(fds.is_empty());
        let (message, fds) = read_client_msg(&receiver).unwrap().unwrap();
        assert_eq!(message, second);
        assert_eq!(fds.len(), 4);
        assert!(fds.iter().all(|fd| {
            rustix::io::fcntl_getfd(fd)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        }));
        let (message, fds) = read_client_msg(&receiver).unwrap().unwrap();
        assert_eq!(message, third);
        assert!(fds.is_empty());
    }

    #[test]
    fn accumulated_excess_rights_are_closed_on_rejection() {
        let (mut sender, receiver) = pair();
        let (probe, peer) = UnixDatagram::pair().unwrap();
        let borrowed: Vec<_> = (0..31).map(|_| probe.as_fd()).collect();
        send_chunk(&sender, b"{", &borrowed).unwrap();
        send_chunk(&sender, b" ", &borrowed).unwrap();
        send_chunk(&sender, b" ", &[probe.as_fd()]).unwrap();
        sender.write_all(b"}\n").unwrap();
        drop(probe);
        let error = read_client_msg(&receiver).unwrap_err();
        assert!(error.to_string().contains("too many descriptors"));
        assert!(peer.send(b"closed").is_err(), "received descriptors leaked");
    }

    #[test]
    fn truncated_ancillary_rights_are_closed_on_rejection() {
        let (sender, receiver) = pair();
        let (probe, peer) = UnixDatagram::pair().unwrap();
        let borrowed: Vec<_> = (0..MAX_FDS + 8).map(|_| probe.as_fd()).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS + 8))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(&borrowed)));
        sendmsg(
            &sender,
            &[IoSlice::new(b"{}\n")],
            &mut ancillary,
            SendFlags::NOSIGNAL,
        )
        .unwrap();
        drop(borrowed);
        drop(probe);
        let error = read_client_msg(&receiver).unwrap_err();
        assert!(error.to_string().contains("truncated descriptors"));
        assert!(
            peer.send(b"closed").is_err(),
            "truncated descriptors leaked"
        );
    }

    #[test]
    fn malformed_json_closes_all_received_rights() {
        let (sender, receiver) = pair();
        let (probe, peer) = UnixDatagram::pair().unwrap();
        send_chunk(&sender, b"not-json\n", &[probe.as_fd()]).unwrap();
        drop(probe);
        assert!(matches!(
            read_client_msg(&receiver),
            Err(CodecError::Malformed(_))
        ));
        assert!(
            peer.send(b"closed").is_err(),
            "malformed request leaked descriptors"
        );
    }

    #[test]
    fn maximum_descriptor_vector_arrives_without_truncation() {
        let (sender, receiver) = pair();
        let (probe, peer) = UnixDatagram::pair().unwrap();
        let borrowed: Vec<_> = (0..MAX_FDS).map(|_| probe.as_fd()).collect();
        let message = attach(1, 31, true);
        write_client_msg(&sender, &message, &borrowed).unwrap();
        drop(borrowed);
        drop(probe);
        let (actual, fds) = read_client_msg(&receiver).unwrap().unwrap();
        assert_eq!(actual, message);
        assert_eq!(fds.len(), MAX_FDS);
        assert!(fds.iter().all(|fd| {
            rustix::io::fcntl_getfd(fd)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        }));
        drop(fds);
        assert!(peer.send(b"closed").is_err());
    }

    #[test]
    fn oversized_line_closes_rights_before_parsing() {
        let (mut sender, receiver) = pair();
        let (probe, peer) = UnixDatagram::pair().unwrap();
        let sending = std::thread::spawn(move || {
            send_chunk(&sender, b"{", &[probe.as_fd()]).unwrap();
            drop(probe);
            sender.write_all(&vec![b' '; MAX_LINE_BYTES]).unwrap();
            sender.write_all(b"}\n").unwrap();
        });
        let error = read_client_msg(&receiver).unwrap_err();
        assert!(matches!(error, CodecError::LineTooLong));
        sending.join().unwrap();
        assert!(
            peer.send(b"closed").is_err(),
            "oversized request leaked descriptors"
        );
    }

    #[test]
    fn eof_without_newline_keeps_the_legacy_codec_contract() {
        let (mut sender, receiver) = pair();
        sender
            .write_all(br#"{"type":"hello","proto_version":1}"#)
            .unwrap();
        sender.shutdown(std::net::Shutdown::Write).unwrap();
        let (message, fds) = read_client_msg(&receiver).unwrap().unwrap();
        assert_eq!(message, ClientMsg::Hello { proto_version: 1 });
        assert!(fds.is_empty());
        assert!(read_client_msg(&receiver).unwrap().is_none());
    }
}
