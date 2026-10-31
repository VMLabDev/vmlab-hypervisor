//! swtpm's documented socket control protocol (`swtpm_ioctls(3)`).

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub(super) const MAX_PACKET: usize = 4096;
const MAX_BLOB: usize = 16 * 1024 * 1024;

pub(super) struct Backend {
    control: UnixStream,
    data: UnixStream,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn word(stream: &mut UnixStream) -> io::Result<u32> {
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

impl Backend {
    pub(super) fn connect(path: &Path) -> io::Result<Self> {
        let control = UnixStream::connect(path)?;
        let (data, remote) = UnixStream::pair()?;
        for stream in [&control, &data] {
            stream.set_read_timeout(Some(Duration::from_secs(30)))?;
            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        }
        let mut backend = Self { control, data };
        let mut command = 0x10u32.to_be_bytes();
        let mut iov = libc::iovec {
            iov_base: command.as_mut_ptr().cast(),
            iov_len: command.len(),
        };
        // Align the ancillary buffer for cmsghdr.
        let mut ancillary = [0usize; 8];
        // SAFETY: all pointers describe live, correctly aligned buffers for sendmsg.
        let sent = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = ancillary.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
            std::ptr::write(libc::CMSG_DATA(cmsg).cast::<i32>(), remote.as_raw_fd());
            libc::sendmsg(backend.control.as_raw_fd(), &msg, libc::MSG_NOSIGNAL)
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        backend.control.write_all(&command[sent as usize..])?;
        backend.result()?;
        backend.call(2, &0u32.to_be_bytes())?;
        Ok(backend)
    }

    fn result(&mut self) -> io::Result<()> {
        let result = word(&mut self.control)?;
        if result != 0 {
            return Err(invalid(format!("swtpm control result {result:#x}")));
        }
        Ok(())
    }

    fn call(&mut self, command: u32, payload: &[u8]) -> io::Result<()> {
        let mut request = command.to_be_bytes().to_vec();
        request.extend_from_slice(payload);
        self.control.write_all(&request)?;
        self.result()
            .map_err(|e| invalid(format!("command {command:#x}: {e}")))
    }

    pub(super) fn established(&mut self) -> io::Result<bool> {
        self.call(4, &[])?;
        let mut bit = [0; 4];
        self.control.read_exact(&mut bit)?;
        Ok(bit[0] != 0)
    }

    pub(super) fn reset(&mut self) -> io::Result<()> {
        self.call(0x0e, &[])?;
        self.call(2, &1u32.to_be_bytes())
    }

    pub(super) fn reset_established(&mut self, locality: u8) -> io::Result<()> {
        self.call(0x0b, &[locality])
    }

    pub(super) fn hash_start(&mut self) -> io::Result<()> {
        self.call(6, &[])
    }

    pub(super) fn hash_data(&mut self, data: &[u8]) -> io::Result<()> {
        let mut request = (data.len() as u32).to_be_bytes().to_vec();
        request.extend_from_slice(data);
        self.call(7, &request)
    }

    pub(super) fn hash_end(&mut self) -> io::Result<()> {
        self.call(8, &[])
    }

    pub(super) fn execute(&mut self, locality: u8, request: &[u8]) -> io::Result<Vec<u8>> {
        self.call(5, &[locality])?;
        self.data.write_all(request)?;
        let mut response = vec![0; 10];
        self.data.read_exact(&mut response)?;
        let length = u32::from_be_bytes(response[2..6].try_into().unwrap()) as usize;
        if !(10..=MAX_PACKET).contains(&length) {
            return Err(invalid("invalid swtpm response length"));
        }
        response.resize(length, 0);
        self.data.read_exact(&mut response[10..])?;
        Ok(response)
    }

    pub(super) fn save(&mut self) -> io::Result<Vec<Vec<u8>>> {
        let mut blobs = Vec::new();
        for kind in 1u32..=3 {
            let mut request = 1u32.to_be_bytes().to_vec();
            request.extend_from_slice(&kind.to_be_bytes());
            request.extend_from_slice(&0u32.to_be_bytes());
            let mut message = 0x0cu32.to_be_bytes().to_vec();
            message.extend_from_slice(&request);
            self.control.write_all(&message)?;
            let result = word(&mut self.control)?;
            let flags = word(&mut self.control)?;
            let total = word(&mut self.control)? as usize;
            let length = word(&mut self.control)? as usize;
            // TPM_RETRY with no data denotes an absent optional savestate blob.
            if kind == 3 && result == 0x800 && total == 0 && length == 0 {
                blobs.push(Vec::new());
                continue;
            }
            if result != 0 {
                return Err(invalid(format!("swtpm blob {kind} result {result:#x}")));
            }
            if flags & 2 != 0 || total > MAX_BLOB || length != total {
                return Err(invalid("unsupported swtpm state blob"));
            }
            let mut blob = vec![0; length];
            self.control.read_exact(&mut blob)?;
            blobs.push(blob);
        }
        Ok(blobs)
    }

    pub(super) fn restore(&mut self, blobs: &[Vec<u8>]) -> io::Result<()> {
        if blobs.len() != 3 || blobs.iter().any(|b| b.len() > MAX_BLOB) {
            return Err(invalid("invalid swtpm snapshot blobs"));
        }
        self.call(0x0e, &[])?;
        for (index, blob) in blobs.iter().enumerate() {
            let mut request = 0u32.to_be_bytes().to_vec();
            request.extend_from_slice(&(index as u32 + 1).to_be_bytes());
            request.extend_from_slice(&(blob.len() as u32).to_be_bytes());
            request.extend_from_slice(blob);
            self.call(0x0d, &request)?;
        }
        self.call(2, &0u32.to_be_bytes())
    }

    pub(super) fn shutdown(&mut self) -> io::Result<()> {
        self.call(3, &[])
    }
}
