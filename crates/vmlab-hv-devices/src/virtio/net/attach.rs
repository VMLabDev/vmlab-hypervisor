//! Control-plane ownership of passed network descriptors.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use parking_lot::Mutex;

use super::{Net, NetConfig, Shared, notify_worker, tap::TapBackend};
use crate::{GuestMemory, VirtioDevice};

#[derive(Clone)]
pub struct NetBackendHandle {
    shared: Arc<Mutex<Shared>>,
    wake: Arc<UnixStream>,
    pending: Arc<std::sync::atomic::AtomicBool>,
    memory: GuestMemory,
    pairs: u16,
    offered: u64,
}

impl NetBackendHandle {
    fn install(&self, pairs: u16, vhost: bool, fds: Vec<OwnedFd>) -> io::Result<()> {
        if pairs != self.pairs {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "queue count differs from configured TAP attachment",
            ));
        }
        let mut shared = self.shared.lock();
        if shared.tap.is_some() || shared.mem.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "network backend is already installed or activated",
            ));
        }
        let backend = TapBackend::new(fds, pairs, vhost, self.memory.clone(), self.offered)?;
        shared.tap = Some(backend);
        drop(shared);
        notify_worker(&self.wake, &self.pending);
        Ok(())
    }

    fn ready(&self) -> bool {
        let state = self.shared.lock();
        state.tap.is_some() && state.tap_error.is_none()
    }
}

/// Contains only NICs declared as passed-descriptor attachments.
#[derive(Clone, Default)]
pub struct NetBackendRegistry(Arc<Mutex<BTreeMap<String, NetBackendHandle>>>);

impl std::fmt::Debug for NetBackendRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NetBackendRegistry")
            .field("nics", &self.0.lock().keys().collect::<Vec<_>>())
            .finish()
    }
}

impl NetBackendRegistry {
    pub fn register(&self, mac: String, handle: NetBackendHandle) -> io::Result<()> {
        use std::collections::btree_map::Entry;
        match self.0.lock().entry(mac.to_ascii_lowercase()) {
            Entry::Vacant(entry) => {
                entry.insert(handle);
                Ok(())
            }
            Entry::Occupied(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "duplicate passed-descriptor NIC MAC",
            )),
        }
    }

    pub fn install(&self, mac: &str, pairs: u16, vhost: bool, fds: Vec<OwnedFd>) -> io::Result<()> {
        let handle = self
            .0
            .lock()
            .get(&mac.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no NIC is configured for these passed descriptors",
                )
            })?;
        handle.install(pairs, vhost, fds)
    }

    pub fn ready(&self) -> bool {
        self.0.lock().values().all(NetBackendHandle::ready)
    }
}

impl Net {
    /// Use completed packets when the kernel fabric cannot carry offload metadata.
    pub fn with_tap_raw(
        config: NetConfig,
        pairs: u16,
        memory: GuestMemory,
    ) -> io::Result<(Self, NetBackendHandle)> {
        let (net, mut handle) = Self::with_tap(config, pairs, memory)?;
        handle.offered &= !super::F_NET_OFFLOADS;
        Ok((net, handle))
    }

    /// Build a NIC whose daemon installs TAP queues before the first resume.
    pub fn with_tap(
        config: NetConfig,
        pairs: u16,
        memory: GuestMemory,
    ) -> io::Result<(Self, NetBackendHandle)> {
        let net = Self::with_offload(config, pairs)?;
        net.shared.lock().tap_mode = true;
        let handle = NetBackendHandle {
            shared: net.shared.clone(),
            wake: Arc::new(net.wake.try_clone()?),
            pending: net.wake_pending.clone(),
            memory,
            pairs,
            offered: net.features(),
        };
        net.wake_worker();
        Ok((net, handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_is_ready_and_unknown_attachment_closes_fds() {
        let registry = NetBackendRegistry::default();
        assert!(registry.ready());
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let error = registry
            .install("00:00:00:00:00:00", 1, false, vec![socket.into()])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let mut byte = [0];
        assert_eq!(std::io::Read::read(&mut peer, &mut byte).unwrap(), 0);
    }

    #[test]
    fn raw_fabric_masks_offloads_and_preserves_multiqueue() {
        use vm_memory::GuestAddress;
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 65536)]).unwrap();
        let (_net, handle) = Net::with_tap_raw(
            NetConfig {
                id: "raw0".into(),
                mac: [2, 0, 0, 0, 0, 1],
                mtu: 1500,
                switch: Default::default(),
            },
            2,
            memory,
        )
        .unwrap();
        assert_eq!(handle.offered & super::super::F_NET_OFFLOADS, 0);
        assert_ne!(handle.offered & super::super::F_MQ, 0);
    }

    #[test]
    fn failed_attachment_does_not_make_the_registry_ready() {
        use vm_memory::GuestAddress;
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 65536)]).unwrap();
        let (net, handle) = Net::with_tap(
            NetConfig {
                id: "net0".into(),
                mac: [2, 0, 0, 0, 0, 1],
                mtu: 1500,
                switch: Default::default(),
            },
            2,
            memory,
        )
        .unwrap();
        let registry = NetBackendRegistry::default();
        registry
            .register("02:00:00:00:00:01".into(), handle.clone())
            .unwrap();
        assert!(
            registry
                .register("02:00:00:00:00:01".into(), handle)
                .is_err()
        );
        assert!(!registry.ready());
        assert!(
            registry
                .install("02:00:00:00:00:01", 1, false, vec![])
                .is_err()
        );
        let (socket, _peer) = UnixStream::pair().unwrap();
        assert!(
            registry
                .install("02:00:00:00:00:01", 2, false, vec![socket.into()])
                .is_err()
        );
        assert!(!registry.ready());
        drop(net);
    }
}
