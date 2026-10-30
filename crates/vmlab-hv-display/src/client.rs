use crate::{Capabilities, Message, Packet, invalid, protocol::VERSION, receive, send};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

/// Blocking native client. Received descriptors belong to the returned Packet.
pub struct Client {
    stream: UnixStream,
    read_lock: Mutex<()>,
    write_lock: Mutex<()>,
    pub capabilities: Capabilities,
}
impl Client {
    pub fn connect(path: impl AsRef<Path>, capabilities: Capabilities) -> io::Result<Self> {
        Self::from_stream(UnixStream::connect(path)?, capabilities)
    }
    pub fn from_stream(stream: UnixStream, capabilities: Capabilities) -> io::Result<Self> {
        send(
            &stream,
            &Message::Hello {
                version: VERSION,
                capabilities,
            },
            &[],
        )?;
        let reply = receive(&stream)?;
        let Message::Hello {
            version: VERSION,
            capabilities: offered,
        } = reply.message
        else {
            return Err(invalid("display version handshake failed"));
        };
        if offered.0 & !capabilities.0 != 0 {
            return Err(invalid("server selected unrequested capabilities"));
        }
        Ok(Self {
            stream,
            read_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
            capabilities: offered,
        })
    }
    pub fn receive(&self) -> io::Result<Packet> {
        let _guard = self.read_lock.lock().unwrap();
        receive(&self.stream)
    }
    pub fn send(&self, message: &Message) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        send(&self.stream, message, &[])
    }
    pub fn release(&self, surface_id: u64, buffer: u32, seq: u64) -> io::Result<()> {
        self.send(&Message::Release {
            surface_id,
            buffer,
            seq,
        })
    }
    pub fn cpu_copy(&self, surface_id: u64) -> io::Result<()> {
        self.send(&Message::CpuCopy { surface_id })
    }
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }
}
