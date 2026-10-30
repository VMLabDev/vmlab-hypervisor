use crate::{Message, invalid, protocol::MAX_MESSAGE};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

#[derive(Debug)]
pub struct Packet {
    pub message: Message,
    pub fds: Vec<OwnedFd>,
}

fn fd_count(message: &Message, count: usize) -> io::Result<()> {
    let valid = match message {
        Message::SurfaceConfigure { buffers, .. } => {
            count == *buffers as usize && (1..=16).contains(buffers)
        }
        Message::FrameReady { .. } => count <= 1,
        _ => count == 0,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid("wrong descriptor count"))
    }
}

/// Send one message. Callers must serialize writes on the same stream.
pub fn send(mut stream: &UnixStream, message: &Message, fds: &[RawFd]) -> io::Result<()> {
    fd_count(message, fds.len())?;
    let bytes = message.encode()?;
    // SAFETY: CMSG_SPACE only computes the size of an ancillary buffer.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut control = vec![0usize; space.div_ceil(std::mem::size_of::<usize>())];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: The zero representation is valid for msghdr; pointers are set below.
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if !fds.is_empty() {
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = space;
        // SAFETY: control is aligned and has CMSG_SPACE bytes for these descriptors.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as usize;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
        }
    }
    let n = loop {
        // SAFETY: All buffers outlive this syscall and describe valid initialized memory.
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    if n == 0 {
        return Err(io::ErrorKind::WriteZero.into());
    }
    stream.write_all(&bytes[n..])
}

/// Receive exactly one message with close-on-exec, RAII-owned descriptors.
pub fn receive(mut stream: &UnixStream) -> io::Result<Packet> {
    let mut head = [0u8; 8];
    let mut fds = Vec::new();
    let mut offset = 0;
    while offset < head.len() {
        let mut control = [0usize; 32];
        let mut iov = libc::iovec {
            iov_base: head[offset..].as_mut_ptr().cast(),
            iov_len: head.len() - offset,
        };
        // SAFETY: A zeroed msghdr is valid. Both buffers live through recvmsg.
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = std::mem::size_of_val(&control);
        // SAFETY: The header points to writable buffers with their correct lengths.
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        // SAFETY: recvmsg populated the ancillary chain. Each received fd is owned once.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&hdr);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let count = ((*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize)
                        / std::mem::size_of::<RawFd>();
                    for i in 0..count {
                        fds.push(OwnedFd::from_raw_fd(
                            *libc::CMSG_DATA(cmsg).cast::<RawFd>().add(i),
                        ));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&hdr, cmsg);
            }
        }
        if hdr.msg_flags & libc::MSG_CTRUNC != 0 || fds.len() > 16 {
            return Err(invalid("ancillary data truncated or too many fds"));
        }
        offset += n as usize;
    }
    let len = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
    if !(8..=MAX_MESSAGE).contains(&len) {
        return Err(invalid("invalid message length"));
    }
    let mut bytes = vec![0; len];
    bytes[..8].copy_from_slice(&head);
    stream.read_exact(&mut bytes[8..])?;
    let message = Message::decode(&bytes)?;
    fd_count(&message, fds.len())?;
    Ok(Packet { message, fds })
}
