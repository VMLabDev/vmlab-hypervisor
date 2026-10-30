// Copyright (C) 2020-2022 Alibaba Cloud. All rights reserved.
// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! The FUSE server stack, lifted from fuse-backend-rs 0.14.0 — the published
//! successor of crosvm's BSD `fuse` crate — under its BSD-3-Clause licence.
//!
//! Adapted for this crate: the transport's `Reader`/`Writer` are built on
//! this workspace's vm-memory 0.18 and virtio-queue 0.18, and the fusedev
//! and vhost-user transport variants are gone — only the virtqueue path
//! remains. See THIRD_PARTY_NOTICES.
//!
//! - [`abi`] is the Linux FUSE wire format (kernel ABI 7.x) plus the
//!   virtio-fs DAX extensions.
//! - [`filesystem`] is the trait a FUSE filesystem implements.
//! - [`server`] decodes a request from a [`transport::Reader`] and writes
//!   the reply through a [`transport::Writer`].
//
// The lift keeps upstream's safety prose, which clippy's
// `undocumented_unsafe_blocks` lint does not always recognise.
#![allow(clippy::undocumented_unsafe_blocks)]

pub mod abi;
pub mod file_buf;
pub mod file_traits;
pub mod filesystem;
pub mod server;
pub mod transport;

use std::ffi::CStr;
use std::io;

pub use filesystem::FileSystem;
pub use server::Server;
pub use transport::{Reader, Writer};

/// Bitmap tracking dirtied pages in guest memory, as vm-memory defines it.
pub use vm_memory::bitmap::BitmapSlice;

/// Error codes for Fuse related operations, lifted from fuse-backend-rs.
#[derive(Debug)]
pub enum Error {
    /// Failed to decode protocol messages.
    DecodeMessage(io::Error),
    /// Failed to encode protocol messages.
    EncodeMessage(io::Error),
    /// One or more parameters are missing.
    MissingParameter,
    /// A C string parameter is invalid.
    InvalidCString(std::ffi::FromBytesWithNulError),
    /// The `len` field of the header is too small.
    InvalidHeaderLength,
    /// The `size` field of the `SetxattrIn` message does not match the length
    /// of the decoded value.
    InvalidXattrSize((u32, usize)),
    /// Invalid message that the server cannot handle properly.
    InvalidMessage(io::Error),
    /// Failed to write buffer to writer.
    FailedToWrite(io::Error),
    /// Failed to split a writer.
    FailedToSplitWriter(transport::Error),
    /// Failed to remap uid/gid.
    FailedToRemapID((u32, u32)),
}

impl std::error::Error for Error {}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        use Error::*;
        match self {
            DecodeMessage(err) => write!(f, "failed to decode fuse message: {err}"),
            EncodeMessage(err) => write!(f, "failed to encode fuse message: {err}"),
            MissingParameter => write!(f, "one or more parameters are missing"),
            InvalidHeaderLength => write!(f, "the `len` field of the header is too small"),
            InvalidCString(err) => write!(f, "a c string parameter is invalid: {err}"),
            InvalidXattrSize((size, len)) => write!(
                f,
                "The `size` field of the `SetxattrIn` message does not match the length of the \
                 decoded value: size = {size}, value.len() = {len}"
            ),
            InvalidMessage(err) => write!(f, "cannot process fuse message: {err}"),
            FailedToWrite(err) => write!(f, "cannot write to buffer: {err}"),
            FailedToSplitWriter(err) => write!(f, "cannot split a writer: {err}"),
            FailedToRemapID((uid, gid)) => write!(
                f,
                "failed to remap the context of user (uid={uid}, gid={gid})."
            ),
        }
    }
}

/// Result for Fuse related operations.
pub type Result<T> = ::std::result::Result<T, Error>;

/// Trim all trailing nul terminators.
pub fn bytes_to_cstr(buf: &[u8]) -> Result<&CStr> {
    // There might be multiple 0s at the end of buf, find & use the first one and trim other zeros.
    match buf.iter().position(|x| *x == 0) {
        // Convert to a `CStr` so that we can drop the '\0' byte at the end and make sure
        // there are no interior '\0' bytes.
        Some(pos) => CStr::from_bytes_with_nul(&buf[0..=pos]).map_err(Error::InvalidCString),
        None => {
            // Invalid input, just call CStr::from_bytes_with_nul() for suitable error code
            CStr::from_bytes_with_nul(buf).map_err(Error::InvalidCString)
        }
    }
}

/// Convert an `io::Error` to a raw errno for FUSE replies.
pub fn encode_io_error_kind(kind: io::ErrorKind) -> libc::c_int {
    use io::ErrorKind::*;
    match kind {
        //ErrorKind::AddrNotAvailable => libc::EADDRNOTAVAIL,
        //ErrorKind::AddrInUse => libc::EADDRINUSE,
        NotFound => libc::ENOENT,
        Interrupted => libc::EINTR,
        //ErrorKind::InvalidInput => libc::EINVAL,
        //ErrorKind::TimedOut => libc::ETIMEDOUT,
        AlreadyExists => libc::EEXIST,
        WouldBlock => libc::EWOULDBLOCK,
        _ => libc::EIO,
    }
}
