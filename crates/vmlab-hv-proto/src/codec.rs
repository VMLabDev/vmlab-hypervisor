//! The wire codec: one message per JSON line.
//!
//! The control protocol is newline-delimited JSON over a byte stream. These
//! functions work over any [`BufRead`]/[`Write`], so the same codec serves
//! unix sockets, socketpairs and in-memory buffers in tests.
//!
//! Lines longer than [`MAX_LINE_BYTES`] are rejected with
//! [`CodecError::LineTooLong`] as soon as the limit is crossed, without
//! buffering the rest of the line. After such a rejection the stream is no
//! longer at a message boundary and the connection must be closed.

use std::io::{self, BufRead, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::control::{ClientMsg, ServerMsg};

/// Largest message line accepted, in bytes, excluding the terminating newline.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Failures of the line codec.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The stream errored underneath the codec.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    /// The line exceeded [`MAX_LINE_BYTES`]. Framing is lost; the only sane
    /// reaction is to close the connection.
    #[error("message line exceeds 1 MiB ({MAX_LINE_BYTES} bytes)")]
    LineTooLong,
    /// The line was not a message of the expected kind.
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Write one client message as one line and flush it.
pub fn write_client_msg(w: &mut impl Write, msg: &ClientMsg) -> Result<(), CodecError> {
    write_msg(w, msg)
}

/// Write one server message as one line and flush it.
pub fn write_server_msg(w: &mut impl Write, msg: &ServerMsg) -> Result<(), CodecError> {
    write_msg(w, msg)
}

/// Read one client message. `Ok(None)` means the peer closed the stream at a
/// message boundary. A final line without a newline is still a message: a
/// stream that ends mid-message is indistinguishable from one whose last line
/// lacked its newline.
pub fn read_client_msg(r: &mut impl BufRead) -> Result<Option<ClientMsg>, CodecError> {
    read_msg(r)
}

/// Read one server message; see [`read_client_msg`].
pub fn read_server_msg(r: &mut impl BufRead) -> Result<Option<ServerMsg>, CodecError> {
    read_msg(r)
}

fn write_msg<M: Serialize + ?Sized>(w: &mut impl Write, msg: &M) -> Result<(), CodecError> {
    let mut line = serde_json::to_string(msg)?;
    if line.len() > MAX_LINE_BYTES {
        return Err(CodecError::LineTooLong);
    }
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()?;
    Ok(())
}

fn read_msg<M: DeserializeOwned>(r: &mut impl BufRead) -> Result<Option<M>, CodecError> {
    let mut line = Vec::new();
    let newline_seen = read_line(r, &mut line)?;
    if !newline_seen && line.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&line)?))
}

/// Read one newline-terminated line into `buf` (newline stripped), refusing to
/// buffer more than [`MAX_LINE_BYTES`] bytes. Returns whether a newline was
/// seen; on end of stream the bytes read so far stay in `buf`.
fn read_line(r: &mut impl BufRead, buf: &mut Vec<u8>) -> Result<bool, CodecError> {
    buf.clear();
    loop {
        let available = r.fill_buf()?;
        if available.is_empty() {
            return Ok(false);
        }
        match memchr_newline(available) {
            Some(i) => {
                check_len(buf.len() + i)?;
                buf.extend_from_slice(&available[..i]);
                r.consume(i + 1);
                return Ok(true);
            }
            None => {
                check_len(buf.len() + available.len())?;
                buf.extend_from_slice(available);
                let n = available.len();
                r.consume(n);
            }
        }
    }
}

fn memchr_newline(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b == b'\n')
}

fn check_len(len: usize) -> Result<(), CodecError> {
    if len > MAX_LINE_BYTES {
        Err(CodecError::LineTooLong)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ClientMsg, Command, ErrorCode, ServerMsg};
    use std::io::{Cursor, Read};

    #[test]
    fn one_message_one_line() {
        let mut buf = Vec::new();
        write_client_msg(&mut buf, &ClientMsg::Hello { proto_version: 1 }).unwrap();
        assert_eq!(buf, b"{\"type\":\"hello\",\"proto_version\":1}\n");
    }

    #[test]
    fn read_round_trips_through_write() {
        let mut buf = Vec::new();
        let msg = ServerMsg::Reply {
            id: 7,
            outcome: crate::control::Outcome::Error {
                code: ErrorCode::Internal,
                message: "boom".to_owned(),
            },
        };
        write_server_msg(&mut buf, &msg).unwrap();
        assert_eq!(read_server_msg(&mut Cursor::new(&buf)).unwrap(), Some(msg));
    }

    #[test]
    fn clean_eof_is_none() {
        assert_eq!(read_client_msg(&mut Cursor::new(b"")).unwrap(), None);
    }

    #[test]
    fn last_line_without_newline_is_still_a_message() {
        let bytes = b"{\"type\":\"hello\",\"proto_version\":1}";
        let msg = ClientMsg::Hello { proto_version: 1 };
        assert_eq!(read_client_msg(&mut Cursor::new(bytes)).unwrap(), Some(msg));
    }

    #[test]
    fn malformed_line_is_typed() {
        let err = read_client_msg(&mut Cursor::new(b"not json\n")).unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn blank_line_is_malformed() {
        let err = read_server_msg(&mut Cursor::new(b"\n")).unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn wrong_message_kind_is_malformed() {
        // A client message on the server side does not deserialize.
        let err = read_server_msg(&mut Cursor::new(
            b"{\"type\":\"request\",\"id\":1,\"command\":{\"command\":\"status\"}}\n",
        ))
        .unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn line_at_the_limit_is_accepted() {
        let mut line = vec![b' '; MAX_LINE_BYTES];
        let head = b"{\"type\":\"hello\",\"proto_version\":1}";
        line[..head.len()].copy_from_slice(head);
        line.push(b'\n');
        let msg = ClientMsg::Hello { proto_version: 1 };
        assert_eq!(read_client_msg(&mut Cursor::new(&line)).unwrap(), Some(msg));
    }

    #[test]
    fn line_over_the_limit_is_rejected() {
        let mut line = vec![b'a'; MAX_LINE_BYTES + 1];
        line.push(b'\n');
        let err = read_client_msg(&mut Cursor::new(&line)).unwrap_err();
        assert!(matches!(err, CodecError::LineTooLong));
    }

    /// Counts bytes pulled from the underlying reader, to prove the codec
    /// rejects an oversized line instead of slurping it whole.
    struct Counting<R> {
        inner: R,
        read: usize,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    #[test]
    fn oversize_line_is_rejected_without_buffering_it() {
        let oversized = vec![b'a'; 8 * MAX_LINE_BYTES];
        let counting = Counting {
            inner: Cursor::new(oversized),
            read: 0,
        };
        let mut reader = io::BufReader::with_capacity(8 * 1024, counting);
        let err = read_client_msg(&mut reader).unwrap_err();
        assert!(matches!(err, CodecError::LineTooLong));
        // The default-capacity BufReader above the limit, nothing more.
        assert!(reader.get_ref().read <= MAX_LINE_BYTES + 8 * 1024);
    }

    #[test]
    fn writing_an_oversized_message_is_refused() {
        let msg = ClientMsg::Request {
            id: 1,
            command: Command::SnapshotSave {
                name: "x".repeat(MAX_LINE_BYTES),
            },
        };
        let err = write_client_msg(&mut Vec::new(), &msg).unwrap_err();
        assert!(matches!(err, CodecError::LineTooLong));
    }
}
