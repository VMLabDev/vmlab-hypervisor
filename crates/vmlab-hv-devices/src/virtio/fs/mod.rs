//! virtio-fs (VIRTIO 1.2 §5.11, device id 26): the vhost-user front-end and
//! the FUSE server stack it shares with the in-process device.

pub mod fuse;
pub mod vhost_user;

pub use vhost_user::{Fs, FsConfig};
