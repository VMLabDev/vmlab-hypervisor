// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! Host USB passthrough errors, adapted from crosvm's usb_util and usb
//! backend error types.

use std::io;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("parsing descriptors failed")]
    DescriptorParse,
    #[error("reading descriptors from device failed: {0}")]
    DescriptorRead(io::Error),
    #[error("invalid transfer buffer length: {0}")]
    InvalidBufferLength(std::num::TryFromIntError),
    #[error("isochronous transfer requires non-zero buffer and packet size")]
    InvalidIsochronousParameters,
    #[error("USB ioctl 0x{0:x} failed: {1}")]
    IoctlFailed(u32, io::Error),
    #[error("device has been removed")]
    NoDevice,
    #[error("requested descriptor not found")]
    NoSuchDescriptor,
    #[error("claiming interface {0} failed: {1}")]
    ClaimInterface(u8, Box<Error>),
    #[error("setting configuration {0} failed: {1}")]
    SetConfiguration(u8, Box<Error>),
    #[error("setting interface {0} alt {1} failed: {2}")]
    SetInterface(u8, u8, Box<Error>),
    #[error("getting active configuration failed: {0}")]
    GetActiveConfiguration(Box<Error>),
    #[error("transfer on unknown endpoint 0x{0:x}")]
    NoSuchEndpoint(u8),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
