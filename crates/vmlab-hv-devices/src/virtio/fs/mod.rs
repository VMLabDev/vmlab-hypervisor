//! virtio-fs (VIRTIO 1.2 §5.11, device id 26): the vhost-user front-end and
//! the in-process device, which serves a host directory through the FUSE
//! server stack in [`fuse`].

pub mod device;
pub mod fuse;
pub mod passthrough;
pub mod vhost_user;

pub use device::InProcessFs;
pub use vhost_user::{Fs, FsConfig};
