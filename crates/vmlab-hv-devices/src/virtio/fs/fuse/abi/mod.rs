// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! FUSE Application Binary Interfaces (ABI), lifted from fuse-backend-rs.

/// The Linux FUSE wire format.
#[path = "fuse_abi.rs"]
pub mod fuse_abi;

/// The virtio-fs DAX extensions (setupmapping/removemapping).
pub mod virtio_fs;
