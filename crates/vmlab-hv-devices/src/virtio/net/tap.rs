//! Passed TAP queues and their optional kernel vhost-net rings.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use vhost::vhost_kern::{VhostKernBackend, vhost_binding};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use vmlab_hv_proto::net::{Header, MAX_FRAME_LEN};
use vmm_sys_util::eventfd::EventFd;

use crate::GuestMemory;

// Linux UAPI if_tun.h; the encoded ioctl argument size is four bytes.
const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;
const TUNSETQUEUE: libc::c_ulong = 0x4004_54d9;
const TUNGETIFF: libc::c_ulong = 0x8004_54d2;
const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;
const TUN_F_CSUM: u32 = 1;
const TUN_F_TSO4: u32 = 2;
const TUN_F_TSO6: u32 = 4;
const TUN_F_TSO_ECN: u32 = 8;
const TUN_F_USO4: u32 = 0x20;
const TUN_F_USO6: u32 = 0x40;
const IFF_ATTACH_QUEUE: i16 = 0x200;
const IFF_DETACH_QUEUE: i16 = 0x400;
const F_ANY_LAYOUT: u64 = 1 << 27;
const RING_FEATURES: u64 =
    super::F_INDIRECT_DESC | super::F_EVENT_IDX | super::F_VERSION_1 | super::F_MRG_RXBUF;

/// A borrowed ioctl view; the backend retains every mapping and descriptor.
struct Kernel<'a> {
    file: &'a File,
    memory: &'a GuestMemory,
}

impl AsRawFd for Kernel<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl<'a> VhostKernBackend for Kernel<'a> {
    type AS = &'a GuestMemory;

    fn mem(&self) -> &Self::AS {
        &self.memory
    }

    fn is_valid(&self, config: &VringConfigData) -> bool {
        validate_ring(self.memory, config).is_ok()
    }
}

fn kernel_result<T>(result: vhost::Result<T>) -> io::Result<T> {
    result.map_err(io::Error::other)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_ring(memory: &GuestMemory, config: &VringConfigData) -> io::Result<()> {
    let size = config.queue_size;
    if size == 0 || !size.is_power_of_two() || size > config.queue_max_size {
        return Err(invalid("invalid vhost queue size"));
    }
    let size = usize::from(size);
    for (address, length, alignment) in [
        (config.desc_table_addr, 16 * size, 16),
        (config.avail_ring_addr, 6 + 2 * size, 2),
        (config.used_ring_addr, 6 + 8 * size, 4),
    ] {
        if address % alignment != 0 || memory.get_slice(GuestAddress(address), length).is_err() {
            return Err(invalid(
                "vhost ring must fit one contiguous guest-memory mapping",
            ));
        }
    }
    if !config.is_log_addr_valid() {
        return Err(invalid("invalid vhost logging address"));
    }
    Ok(())
}

fn queue_config(queue: &Queue) -> VringConfigData {
    VringConfigData {
        queue_max_size: super::QUEUE_SIZE,
        queue_size: queue.size(),
        flags: 0,
        desc_table_addr: queue.desc_table(),
        used_ring_addr: queue.used_ring(),
        avail_ring_addr: queue.avail_ring(),
        log_addr: None,
    }
}

struct TapInfo {
    name: [libc::c_char; libc::IFNAMSIZ],
    flags: i32,
}

struct KernelSetup<'a> {
    memory: &'a GuestMemory,
    features: u64,
    regions: &'a [VhostUserMemoryRegionInfo],
    rings: [&'a Queue; 2],
    kick: &'a [EventFd; 2],
    call: &'a [EventFd; 2],
}

/// Tests replace the ioctl boundary while retaining real owned fds and packet I/O.
trait HostOps: Send + Sync {
    fn tap_info(&self, tap: &File) -> io::Result<TapInfo>;
    fn header_size(&self, tap: &File, length: usize) -> io::Result<()>;
    fn offload(&self, tap: &File, flags: u32) -> io::Result<()>;
    fn queue_state(&self, tap: &File, enabled: bool) -> io::Result<()>;
    fn features(&self, file: &File, memory: &GuestMemory) -> io::Result<u64>;
    fn owner(&self, file: &File, memory: &GuestMemory) -> io::Result<()>;
    fn reset_owner(&self, file: &File, memory: &GuestMemory) -> io::Result<()>;
    fn setup(&self, file: &File, setup: KernelSetup<'_>) -> io::Result<()>;
    fn backend(
        &self,
        file: &File,
        memory: &GuestMemory,
        ring: usize,
        tap: Option<RawFd>,
    ) -> io::Result<()>;
    fn base(&self, file: &File, memory: &GuestMemory, ring: usize) -> io::Result<u32>;

    fn close_kernel(&self, file: File) {
        drop(file);
    }
}

struct Syscalls;

impl HostOps for Syscalls {
    fn tap_info(&self, tap: &File) -> io::Result<TapInfo> {
        // SAFETY: zero is a valid initial representation of this UAPI structure.
        let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
        // SAFETY: the owned fd and writable ifreq remain live through the ioctl.
        if unsafe { libc::ioctl(tap.as_raw_fd(), TUNGETIFF, &mut request) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: TUNGETIFF initializes the flags union member on success.
        let flags = unsafe { request.ifr_ifru.ifru_flags } as i32;
        Ok(TapInfo {
            name: request.ifr_name,
            flags,
        })
    }

    fn header_size(&self, tap: &File, length: usize) -> io::Result<()> {
        let length = length as libc::c_int;
        // SAFETY: the ioctl reads one live integer through this pointer.
        if unsafe { libc::ioctl(tap.as_raw_fd(), TUNSETVNETHDRSZ, &length) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn offload(&self, tap: &File, flags: u32) -> io::Result<()> {
        // SAFETY: TUNSETOFFLOAD consumes an integer value, not a pointer.
        if unsafe { libc::ioctl(tap.as_raw_fd(), TUNSETOFFLOAD, libc::c_ulong::from(flags)) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn queue_state(&self, tap: &File, enabled: bool) -> io::Result<()> {
        // SAFETY: zero is a valid initial representation of this UAPI structure.
        let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
        request.ifr_ifru.ifru_flags = if enabled {
            IFF_ATTACH_QUEUE
        } else {
            IFF_DETACH_QUEUE
        };
        // SAFETY: the owned TAP and initialized request remain live through the ioctl.
        if unsafe { libc::ioctl(tap.as_raw_fd(), TUNSETQUEUE, &request) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn features(&self, file: &File, memory: &GuestMemory) -> io::Result<u64> {
        kernel_result(Kernel { file, memory }.get_features())
    }

    fn owner(&self, file: &File, memory: &GuestMemory) -> io::Result<()> {
        kernel_result(Kernel { file, memory }.set_owner())
    }

    fn reset_owner(&self, file: &File, memory: &GuestMemory) -> io::Result<()> {
        kernel_result(Kernel { file, memory }.reset_owner())
    }

    fn setup(&self, file: &File, setup: KernelSetup<'_>) -> io::Result<()> {
        let kernel = Kernel {
            file,
            memory: setup.memory,
        };
        let mask = kernel_result(kernel.get_features())?;
        // TAP supplies virtio-net headers. Do not set VHOST_NET_F_VIRTIO_NET_HDR.
        kernel_result(kernel.set_features(setup.features & mask & RING_FEATURES))?;
        kernel_result(kernel.set_mem_table(setup.regions))?;
        for local in 0..2 {
            let queue = setup.rings[local];
            kernel_result(kernel.set_vring_num(local, queue.size()))?;
            kernel_result(kernel.set_vring_addr(local, &queue_config(queue)))?;
            kernel_result(kernel.set_vring_base(local, queue.next_avail()))?;
            kernel_result(kernel.set_vring_kick(local, &setup.kick[local]))?;
            kernel_result(kernel.set_vring_call(local, &setup.call[local]))?;
        }
        Ok(())
    }

    fn backend(
        &self,
        file: &File,
        _memory: &GuestMemory,
        ring: usize,
        tap: Option<RawFd>,
    ) -> io::Result<()> {
        let request = vhost_binding::vhost_vring_file {
            index: ring as u32,
            fd: tap.unwrap_or(-1),
        };
        // SAFETY: the typed UAPI request and owned vhost fd remain live through the ioctl.
        if unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                vhost_binding::VHOST_NET_SET_BACKEND(),
                &request,
            )
        } < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn base(&self, file: &File, memory: &GuestMemory, ring: usize) -> io::Result<u32> {
        kernel_result(Kernel { file, memory }.get_vring_base(ring))
    }
}

fn receive_offloads(features: u64) -> u32 {
    if features & super::F_GUEST_CSUM == 0 {
        return 0;
    }
    let mut mask = TUN_F_CSUM;
    if features & super::F_GUEST_TSO4 != 0 {
        mask |= TUN_F_TSO4;
    }
    if features & super::F_GUEST_TSO6 != 0 {
        mask |= TUN_F_TSO6;
    }
    if features & super::F_GUEST_ECN != 0 && mask & (TUN_F_TSO4 | TUN_F_TSO6) != 0 {
        mask |= TUN_F_TSO_ECN;
    }
    // Linux exposes one shared UDP_L4 capability, requiring both TUN bits.
    if features & (super::F_GUEST_USO4 | super::F_GUEST_USO6)
        == super::F_GUEST_USO4 | super::F_GUEST_USO6
    {
        mask |= TUN_F_USO4 | TUN_F_USO6;
    }
    mask
}

struct Pair {
    tap: File,
    kernel: Option<File>,
    owner: bool,
    configured: bool,
    queue_attached: bool,
    attached: [bool; 2],
    tx_waiting: bool,
    kick: [EventFd; 2],
    call: [EventFd; 2],
}

pub(super) struct TapBackend {
    // Drop closes kernel resources before the retained memory mappings.
    pairs: Vec<Pair>,
    memory: GuestMemory,
    available: u64,
    header_len: usize,
    ops: Arc<dyn HostOps>,
}

impl TapBackend {
    pub(super) fn new(
        fds: Vec<OwnedFd>,
        count: u16,
        vhost: bool,
        memory: GuestMemory,
        offered: u64,
    ) -> io::Result<Self> {
        Self::with_ops(fds, count, vhost, memory, offered, Arc::new(Syscalls))
    }

    fn with_ops(
        fds: Vec<OwnedFd>,
        count: u16,
        vhost: bool,
        memory: GuestMemory,
        offered: u64,
        ops: Arc<dyn HostOps>,
    ) -> io::Result<Self> {
        let count = usize::from(count);
        if count == 0 || count > 31 || fds.len() != count * if vhost { 2 } else { 1 } {
            return Err(invalid("TAP descriptor count differs from queue pairs"));
        }
        let mut fds = fds.into_iter();
        let taps: Vec<File> = fds.by_ref().take(count).map(File::from).collect();
        let mut kernels = fds.map(File::from);
        let mut result = Self {
            pairs: Vec::with_capacity(count),
            memory,
            available: offered,
            header_len: 12,
            ops,
        };
        let mut name = None;
        for tap in taps {
            let info = result.ops.tap_info(&tap)?;
            let required =
                libc::IFF_TAP | libc::IFF_VNET_HDR | libc::IFF_MULTI_QUEUE | libc::IFF_NO_PI;
            if info.flags & required != required {
                return Err(invalid("TAP requires NO_PI, VNET_HDR and MULTI_QUEUE"));
            }
            if info.flags & i32::from(IFF_DETACH_QUEUE) != 0 {
                return Err(invalid("passed TAP queue is detached"));
            }
            if name.is_some_and(|name| name != info.name) {
                return Err(invalid("TAP queue pairs must belong to one interface"));
            }
            name = Some(info.name);
            // SAFETY: F_GETFL/F_SETFL only inspect/update this owned file description.
            let status = unsafe { libc::fcntl(tap.as_raw_fd(), libc::F_GETFL) };
            if status < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: F_SETFL consumes an integer flag word on this owned descriptor.
            let changed =
                unsafe { libc::fcntl(tap.as_raw_fd(), libc::F_SETFL, status | libc::O_NONBLOCK) };
            if changed < 0 {
                return Err(io::Error::last_os_error());
            }
            let base = TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_TSO_ECN;
            let probe = if offered & super::F_NET_OFFLOADS != 0 {
                result.ops.offload(&tap, base | TUN_F_USO4 | TUN_F_USO6)
            } else {
                Ok(())
            };
            let supported = match probe {
                Ok(()) => Ok(()),
                Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                    result.available &=
                        !(super::F_HOST_USO | super::F_GUEST_USO4 | super::F_GUEST_USO6);
                    result.ops.offload(&tap, base)
                }
                Err(error) => Err(error),
            };
            // Capability discovery must not leave unnegotiated offloads enabled.
            result.ops.offload(&tap, 0)?;
            supported?;
            result.ops.header_size(&tap, 12)?;
            let kernel = if vhost { kernels.next() } else { None };
            if let Some(file) = &kernel {
                let mask = result.ops.features(file, &result.memory)?;
                result.available &= mask | !RING_FEATURES;
            }
            result.pairs.push(Pair {
                tap,
                kernel,
                owner: false,
                configured: false,
                queue_attached: true,
                attached: [false; 2],
                tx_waiting: false,
                kick: [
                    EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?,
                    EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?,
                ],
                call: [
                    EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?,
                    EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?,
                ],
            });
        }
        Ok(result)
    }

    pub(super) fn features(&self) -> u64 {
        self.available
    }
    pub(super) fn kernel(&self) -> bool {
        self.pairs.first().is_some_and(|pair| pair.kernel.is_some())
    }

    pub(super) fn configure(
        &mut self,
        queues: &mut [Option<Queue>],
        features: u64,
        active: u16,
    ) -> io::Result<()> {
        self.pause(queues)?;
        if active == 0 || usize::from(active) > self.pairs.len() {
            return Err(invalid("active queue count exceeds TAP queue pairs"));
        }
        // The transitional PCI transport offers ANY_LAYOUT on the device's behalf.
        if features & !(self.available | F_ANY_LAYOUT) != 0 {
            return Err(invalid(
                "negotiated network features exceed backend capabilities",
            ));
        }
        for pair in 0..usize::from(active) {
            for local in 0..2 {
                let queue = queues
                    .get(pair * 2 + local)
                    .and_then(Option::as_ref)
                    .filter(|queue| queue.ready())
                    .ok_or_else(|| invalid("active TAP queue is not configured"))?;
                validate_ring(&self.memory, &queue_config(queue))?;
            }
        }
        self.header_len = super::hdr_len(features);
        let regions = self
            .memory
            .iter()
            .map(|region| {
                let host = self
                    .memory
                    .get_host_address(region.start_addr())
                    .map_err(io::Error::other)?;
                Ok(VhostUserMemoryRegionInfo {
                    guest_phys_addr: region.start_addr().0,
                    memory_size: region.len(),
                    userspace_addr: host as u64,
                    mmap_handle: -1,
                    ..Default::default()
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let operation = (|| {
            for (index, pair) in self.pairs.iter_mut().enumerate() {
                self.ops.header_size(&pair.tap, self.header_len)?;
                self.ops.offload(&pair.tap, receive_offloads(features))?;
                let Some(file) = &pair.kernel else { continue };
                let enabled = index < usize::from(active);
                if enabled != pair.queue_attached {
                    self.ops.queue_state(&pair.tap, enabled)?;
                    pair.queue_attached = enabled;
                }
                if !enabled {
                    continue;
                }
                if !pair.owner {
                    self.ops.owner(file, &self.memory)?;
                    pair.owner = true;
                }
                self.ops.setup(
                    file,
                    KernelSetup {
                        memory: &self.memory,
                        features,
                        regions: &regions,
                        rings: [
                            queues[index * 2].as_ref().unwrap(),
                            queues[index * 2 + 1].as_ref().unwrap(),
                        ],
                        kick: &pair.kick,
                        call: &pair.call,
                    },
                )?;
                pair.configured = true;
                for local in 0..2 {
                    self.ops
                        .backend(file, &self.memory, local, Some(pair.tap.as_raw_fd()))?;
                    pair.attached[local] = true;
                }
                for kick in &pair.kick {
                    kick.write(1)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = operation {
            if let Err(cleanup) = self.pause(queues) {
                return Err(io::Error::other(format!(
                    "configuring vhost-net: {error}; stopping it: {cleanup}"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    /// Detach every backend before reading bases or allowing snapshots to copy RAM.
    pub(super) fn pause(&mut self, queues: &mut [Option<Queue>]) -> io::Result<()> {
        let mut first = None;
        for (index, pair) in self.pairs.iter_mut().enumerate() {
            let Some(file) = &pair.kernel else { continue };
            let was_active = pair.attached.iter().any(|attached| *attached);
            for local in 0..2 {
                if !pair.attached[local] {
                    continue;
                }
                match self.ops.backend(file, &self.memory, local, None) {
                    Ok(()) => pair.attached[local] = false,
                    Err(error) => {
                        first.get_or_insert(error);
                    }
                }
            }
            if was_active && !pair.attached.iter().any(|attached| *attached) && pair.configured {
                for local in 0..2 {
                    match self.ops.base(file, &self.memory, local) {
                        Ok(base) => {
                            if let Some(queue) =
                                queues.get_mut(index * 2 + local).and_then(Option::as_mut)
                            {
                                match self
                                    .memory
                                    .read_obj::<u16>(GuestAddress(queue.used_ring() + 2))
                                {
                                    Ok(used) => {
                                        queue.set_next_avail(base as u16);
                                        queue.set_next_used(u16::from_le(used));
                                    }
                                    Err(error) => {
                                        first.get_or_insert(io::Error::other(error));
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            first.get_or_insert(error);
                        }
                    }
                }
            }
        }
        first.map_or(Ok(()), Err)
    }

    pub(super) fn kick(&self, index: u16) -> io::Result<()> {
        if let Some(pair) = self.pairs.get(usize::from(index / 2)) {
            let local = usize::from(index % 2);
            if pair.attached[local] {
                pair.kick[local].write(1)?;
            }
        }
        Ok(())
    }

    pub(super) fn pollfds(&self) -> Vec<(u16, libc::pollfd)> {
        let mut polls = Vec::new();
        for (index, pair) in self.pairs.iter().enumerate() {
            if self.kernel() {
                for local in 0..2 {
                    if pair.attached[local] {
                        polls.push((
                            (index * 2 + local) as u16,
                            libc::pollfd {
                                fd: pair.call[local].as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ));
                    }
                }
            } else {
                // Userspace drains every TAP and steers to active guest queues.
                polls.push((
                    index as u16,
                    libc::pollfd {
                        fd: pair.tap.as_raw_fd(),
                        events: libc::POLLIN | if pair.tx_waiting { libc::POLLOUT } else { 0 },
                        revents: 0,
                    },
                ));
            }
        }
        polls
    }

    pub(super) fn consume_call(&self, index: u16) -> io::Result<()> {
        let pair = self
            .pairs
            .get(usize::from(index / 2))
            .ok_or_else(|| invalid("unknown vhost queue"))?;
        pair.call[usize::from(index % 2)].read().map(|_| ())
    }

    /// Drain completions after pause too, before the transport snapshots IRQ state.
    pub(super) fn drain_calls(&self) -> io::Result<Vec<u16>> {
        let mut ready = Vec::new();
        for (index, pair) in self.pairs.iter().enumerate() {
            if pair.kernel.is_none() {
                continue;
            }
            for local in 0..2 {
                loop {
                    match pair.call[local].read() {
                        Ok(_) => {
                            ready.push((index * 2 + local) as u16);
                            break;
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(ready)
    }

    pub(super) fn tx_pending(&mut self, pair: u16, pending: bool) -> io::Result<()> {
        self.pairs
            .get_mut(usize::from(pair))
            .ok_or_else(|| invalid("unknown TAP queue"))?
            .tx_waiting = pending;
        Ok(())
    }

    pub(super) fn receive(&mut self, pair: u16) -> io::Result<Option<Vec<u8>>> {
        if self.kernel() {
            return Err(invalid("userspace cannot read a vhost-owned TAP"));
        }
        let file = &mut self
            .pairs
            .get_mut(usize::from(pair))
            .ok_or_else(|| invalid("unknown TAP queue"))?
            .tap;
        let mut body = vec![0; self.header_len + MAX_FRAME_LEN + 1];
        let count = loop {
            match file.read(&mut body) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                result => break result?,
            }
        };
        if count < self.header_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short TAP vnet header",
            ));
        }
        if count > self.header_len + MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized TAP packet",
            ));
        }
        let header = Header::decode(&body[..self.header_len])?;
        let frame = &body[self.header_len..count];
        header.validate(frame)?;
        let mut record = header.metadata(pair).to_vec();
        record.extend_from_slice(frame);
        Ok(Some(record))
    }

    pub(super) fn send(&mut self, pair: u16, header: Header, frame: &[u8]) -> io::Result<()> {
        if self.kernel() {
            return Err(invalid("userspace cannot write a vhost-owned TAP"));
        }
        header.validate(frame)?;
        let encoded = header.encode();
        let slices = [
            io::IoSlice::new(&encoded[..self.header_len]),
            io::IoSlice::new(frame),
        ];
        let pair = self
            .pairs
            .get_mut(usize::from(pair))
            .ok_or_else(|| invalid("unknown TAP queue"))?;
        let written = loop {
            match pair.tap.write_vectored(&slices) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    pair.tx_waiting = true;
                    return Err(error);
                }
                result => break result?,
            }
        };
        pair.tx_waiting = false;
        if written != self.header_len + frame.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short TAP packet write",
            ));
        }
        Ok(())
    }
}

impl Drop for TapBackend {
    fn drop(&mut self) {
        let mut retain_memory = false;
        for pair in &mut self.pairs {
            let Some(file) = pair.kernel.take() else {
                continue;
            };
            for local in 0..2 {
                if !pair.attached[local] {
                    continue;
                }
                match self.ops.backend(&file, &self.memory, local, None) {
                    Ok(()) => pair.attached[local] = false,
                    Err(error) => log::error!("detaching vhost-net ring {local}: {error}"),
                }
            }
            if pair.owner
                && let Err(error) = self.ops.reset_owner(&file, &self.memory)
            {
                log::error!("resetting vhost-net owner: {error}");
                retain_memory |= pair.attached.iter().any(|attached| *attached);
            }
            self.ops.close_kernel(file);
        }
        if retain_memory {
            // A sender may retain another fd for the same open vhost file.
            // Keep RAM mapped if neither detach nor RESET_OWNER stopped DMA.
            log::error!(
                "vhost-net cleanup could not stop DMA; retaining guest RAM until process exit"
            );
            std::mem::forget(self.memory.clone());
        }
    }
}

#[cfg(test)]
pub(super) fn test_backend(
    pairs: u16,
    vhost: bool,
    memory: GuestMemory,
) -> (TapBackend, Vec<std::os::unix::net::UnixDatagram>) {
    tests::packet_fixture(pairs, vhost, memory)
}

#[cfg(test)]
impl TapBackend {
    pub(super) fn test_call(&self, index: u16) {
        self.pairs[usize::from(index / 2)].call[usize::from(index % 2)]
            .write(1)
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::unix::net::{UnixDatagram, UnixStream};
    use std::sync::{Mutex, Weak};
    use vm_memory::{GuestRegionMmap, MmapRegion};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Offload(u32),
        Header(usize),
        Queue(usize, bool),
        Owner(usize),
        Setup(usize, [u16; 2], Vec<(u64, u64, u64)>),
        Backend(usize, usize, bool),
        Base(usize, usize),
        Reset(usize),
        Close(usize, bool),
    }

    #[derive(Default)]
    struct State {
        taps: Vec<RawFd>,
        kernels: Vec<RawFd>,
        calls: Vec<Call>,
        active: BTreeSet<(usize, usize)>,
        fail_attach: Option<(usize, usize)>,
        fail_detach: Option<(usize, usize)>,
        fail_all_detach: bool,
        fail_reset: bool,
        reject_uso: bool,
        memory: Option<Weak<MmapRegion<()>>>,
    }

    #[derive(Default)]
    struct Fake(Mutex<State>);

    fn position(files: &mut Vec<RawFd>, file: &File) -> usize {
        if let Some(index) = files.iter().position(|fd| *fd == file.as_raw_fd()) {
            index
        } else {
            files.push(file.as_raw_fd());
            files.len() - 1
        }
    }

    impl HostOps for Fake {
        fn tap_info(&self, tap: &File) -> io::Result<TapInfo> {
            position(&mut self.0.lock().unwrap().taps, tap);
            Ok(TapInfo {
                name: [0; libc::IFNAMSIZ],
                flags: libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR | libc::IFF_MULTI_QUEUE,
            })
        }
        fn header_size(&self, _tap: &File, length: usize) -> io::Result<()> {
            self.0.lock().unwrap().calls.push(Call::Header(length));
            Ok(())
        }
        fn offload(&self, _tap: &File, flags: u32) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.calls.push(Call::Offload(flags));
            if state.reject_uso && flags & (TUN_F_USO4 | TUN_F_USO6) != 0 {
                Err(io::Error::from_raw_os_error(libc::EINVAL))
            } else {
                Ok(())
            }
        }
        fn queue_state(&self, tap: &File, enabled: bool) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            let pair = position(&mut state.taps, tap);
            state.calls.push(Call::Queue(pair, enabled));
            Ok(())
        }
        fn features(&self, file: &File, _memory: &GuestMemory) -> io::Result<u64> {
            position(&mut self.0.lock().unwrap().kernels, file);
            Ok(RING_FEATURES)
        }
        fn owner(&self, file: &File, _memory: &GuestMemory) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            let pair = position(&mut state.kernels, file);
            state.calls.push(Call::Owner(pair));
            Ok(())
        }
        fn reset_owner(&self, file: &File, memory: &GuestMemory) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            let pair = position(&mut state.kernels, file);
            assert!(memory.get_slice(GuestAddress(0), 1).is_ok());
            state.calls.push(Call::Reset(pair));
            if state.fail_reset {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            state.active.retain(|(index, _)| *index != pair);
            Ok(())
        }
        fn setup(&self, file: &File, setup: KernelSetup<'_>) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            let pair = position(&mut state.kernels, file);
            state.calls.push(Call::Setup(
                pair,
                [setup.rings[0].next_avail(), setup.rings[1].next_avail()],
                setup
                    .regions
                    .iter()
                    .map(|region| {
                        (
                            region.guest_phys_addr,
                            region.memory_size,
                            region.userspace_addr,
                        )
                    })
                    .collect(),
            ));
            Ok(())
        }
        fn backend(
            &self,
            file: &File,
            _memory: &GuestMemory,
            ring: usize,
            tap: Option<RawFd>,
        ) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            let pair = position(&mut state.kernels, file);
            let enabled = tap.is_some();
            state.calls.push(Call::Backend(pair, ring, enabled));
            if enabled && state.fail_attach == Some((pair, ring)) {
                state.fail_attach = None;
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            if !enabled && (state.fail_detach == Some((pair, ring)) || state.fail_all_detach) {
                state.fail_detach = None;
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            if enabled {
                state.active.insert((pair, ring));
            } else {
                state.active.remove(&(pair, ring));
            }
            Ok(())
        }
        fn base(&self, file: &File, _memory: &GuestMemory, ring: usize) -> io::Result<u32> {
            let mut state = self.0.lock().unwrap();
            let pair = position(&mut state.kernels, file);
            state.calls.push(Call::Base(pair, ring));
            Ok((100 + pair * 2 + ring) as u32)
        }
        fn close_kernel(&self, file: File) {
            let mut state = self.0.lock().unwrap();
            let pair = position(&mut state.kernels, &file);
            drop(file);
            let alive = state
                .memory
                .as_ref()
                .is_some_and(|memory| memory.upgrade().is_some());
            state.calls.push(Call::Close(pair, alive));
        }
    }

    fn offered() -> u64 {
        RING_FEATURES
            | super::super::F_CSUM
            | super::super::F_GUEST_CSUM
            | super::super::F_GUEST_TSO4
            | super::super::F_GUEST_TSO6
            | super::super::F_GUEST_ECN
            | super::super::F_HOST_TSO4
            | super::super::F_HOST_TSO6
            | super::super::F_HOST_ECN
            | super::super::F_GUEST_USO4
            | super::super::F_GUEST_USO6
            | super::super::F_HOST_USO
            | super::super::F_MAC
            | super::super::F_STATUS
            | super::super::F_MTU
            | super::super::F_CTRL_VQ
            | super::super::F_MQ
    }

    fn memory() -> GuestMemory {
        GuestMemory::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap()
    }

    fn queues(memory: &GuestMemory, pairs: usize) -> Vec<Option<Queue>> {
        (0..pairs * 2)
            .map(|index| {
                let base = index as u64 * 0x2000;
                let mut queue = Queue::new(super::super::QUEUE_SIZE).unwrap();
                queue.try_set_size(8).unwrap();
                queue
                    .try_set_desc_table_address(GuestAddress(base))
                    .unwrap();
                queue
                    .try_set_avail_ring_address(GuestAddress(base + 0x1000))
                    .unwrap();
                queue
                    .try_set_used_ring_address(GuestAddress(base + 0x1100))
                    .unwrap();
                queue.set_next_avail(3 + index as u16);
                queue.set_ready(true);
                memory
                    .write_obj((70 + index as u16).to_le(), GuestAddress(base + 0x1102))
                    .unwrap();
                Some(queue)
            })
            .collect()
    }

    fn fixture(
        pairs: u16,
        vhost: bool,
        memory: GuestMemory,
    ) -> (TapBackend, Arc<Fake>, Vec<UnixDatagram>, Vec<UnixStream>) {
        let fake = Arc::new(Fake::default());
        fake.0.lock().unwrap().memory =
            Some(Arc::downgrade(&memory.iter().next().unwrap().get_mmap()));
        let mut fds = Vec::new();
        let mut packets = Vec::new();
        let mut kernels = Vec::new();
        for _ in 0..pairs {
            let (socket, peer) = UnixDatagram::pair().unwrap();
            peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .unwrap();
            fds.push(socket.into());
            packets.push(peer);
        }
        if vhost {
            for _ in 0..pairs {
                let (socket, peer) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
                    .unwrap();
                fds.push(socket.into());
                kernels.push(peer);
            }
        }
        let backend =
            TapBackend::with_ops(fds, pairs, vhost, memory, offered(), fake.clone()).unwrap();
        (backend, fake, packets, kernels)
    }

    pub(super) fn packet_fixture(
        pairs: u16,
        vhost: bool,
        memory: GuestMemory,
    ) -> (TapBackend, Vec<UnixDatagram>) {
        let (backend, _, packets, _) = fixture(pairs, vhost, memory);
        (backend, packets)
    }

    #[test]
    fn raw_fabric_never_enables_tun_offloads() {
        let memory = memory();
        let (socket, _peer) = UnixDatagram::pair().unwrap();
        let fake = Arc::new(Fake::default());
        let backend = TapBackend::with_ops(
            vec![socket.into()],
            1,
            false,
            memory,
            offered() & !super::super::F_NET_OFFLOADS,
            fake.clone(),
        )
        .unwrap();
        assert_eq!(backend.features() & super::super::F_NET_OFFLOADS, 0);
        let state = fake.0.lock().unwrap();
        assert!(state.calls.contains(&Call::Offload(0)));
        assert!(
            !state
                .calls
                .iter()
                .any(|call| matches!(call, Call::Offload(flags) if *flags != 0))
        );
    }

    #[test]
    fn kernel_ring_mask_does_not_remove_tap_offloads() {
        let memory = memory();
        let (mut backend, fake, _, _) = fixture(1, true, memory.clone());
        assert_eq!(backend.features(), offered());
        let mut queues = queues(&memory, 1);
        let features =
            super::super::F_VERSION_1 | super::super::F_GUEST_CSUM | super::super::F_GUEST_TSO4;
        fake.0.lock().unwrap().calls.clear();
        backend.configure(&mut queues, features, 1).unwrap();
        let state = fake.0.lock().unwrap();
        assert!(state.calls.contains(&Call::Header(12)));
        assert!(
            state
                .calls
                .contains(&Call::Offload(TUN_F_CSUM | TUN_F_TSO4))
        );
        assert_eq!(state.active.len(), 2);
    }

    #[test]
    fn transport_any_layout_does_not_require_backend_support() {
        for kernel in [false, true] {
            let memory = memory();
            let (mut backend, fake, _, _) = fixture(1, kernel, memory.clone());
            let mut queues = queues(&memory, 1);
            let any_layout = 1 << 27;
            assert_eq!(backend.features() & any_layout, 0);
            backend
                .configure(&mut queues, offered() | any_layout, 1)
                .unwrap();
            if kernel {
                assert_eq!(fake.0.lock().unwrap().active.len(), 2);
            }
            assert!(
                backend
                    .configure(&mut queues, offered() | (1 << 63), 1)
                    .is_err()
            );
        }
    }

    #[test]
    fn receive_offload_mask_follows_guest_negotiation() {
        assert_eq!(receive_offloads(0), 0);
        assert_eq!(
            receive_offloads(super::super::F_CSUM | super::super::F_HOST_TSO4),
            0
        );
        assert_eq!(
            receive_offloads(super::super::F_GUEST_CSUM | super::super::F_GUEST_USO4),
            TUN_F_CSUM
        );
        assert_eq!(
            receive_offloads(
                super::super::F_GUEST_CSUM
                    | super::super::F_GUEST_USO4
                    | super::super::F_GUEST_USO6
            ),
            TUN_F_CSUM | TUN_F_USO4 | TUN_F_USO6
        );
    }

    #[test]
    fn probing_disables_offloads_and_detects_pre_uso_hosts() {
        let fake = Arc::new(Fake::default());
        fake.0.lock().unwrap().reject_uso = true;
        let (socket, _peer) = UnixDatagram::pair().unwrap();
        let backend = TapBackend::with_ops(
            vec![socket.into()],
            1,
            false,
            memory(),
            offered(),
            fake.clone(),
        )
        .unwrap();
        assert_eq!(
            backend.features()
                & (super::super::F_HOST_USO
                    | super::super::F_GUEST_USO4
                    | super::super::F_GUEST_USO6),
            0
        );
        let calls = &fake.0.lock().unwrap().calls;
        assert!(matches!(
            calls
                .iter()
                .rev()
                .find(|call| matches!(call, Call::Offload(_))),
            Some(Call::Offload(0))
        ));
    }

    #[test]
    fn second_ring_attach_failure_rolls_back_every_started_pair() {
        let memory = memory();
        let (mut backend, fake, _, _) = fixture(2, true, memory.clone());
        fake.0.lock().unwrap().fail_attach = Some((1, 1));
        let mut queues = queues(&memory, 2);
        assert!(backend.configure(&mut queues, offered(), 2).is_err());
        let state = fake.0.lock().unwrap();
        assert!(state.active.is_empty());
        assert!(state.calls.contains(&Call::Backend(0, 0, false)));
        assert!(state.calls.contains(&Call::Backend(0, 1, false)));
        assert!(state.calls.contains(&Call::Backend(1, 0, false)));
        assert!(!state.calls.contains(&Call::Backend(1, 1, false)));
        assert!(backend.pollfds().is_empty());
    }

    #[test]
    fn pause_continues_after_one_stop_failure_and_drop_resets_owners() {
        let memory = memory();
        let (mut backend, fake, _, mut kernel_peers) = fixture(2, true, memory.clone());
        let mut queues = queues(&memory, 2);
        backend.configure(&mut queues, offered(), 2).unwrap();
        fake.0.lock().unwrap().fail_detach = Some((0, 0));
        assert!(backend.pause(&mut queues).is_err());
        assert_eq!(fake.0.lock().unwrap().active, BTreeSet::from([(0, 0)]));
        drop(backend);
        assert!(fake.0.lock().unwrap().active.is_empty());
        assert!(fake.0.lock().unwrap().calls.contains(&Call::Reset(0)));
        for peer in &mut kernel_peers {
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn pause_captures_bases_and_used_indices_before_reconfiguration() {
        let memory = memory();
        let (mut backend, fake, _, _) = fixture(2, true, memory.clone());
        let mut queues = queues(&memory, 2);
        backend.configure(&mut queues, offered(), 2).unwrap();
        backend.pause(&mut queues).unwrap();
        for (index, queue) in queues.iter().enumerate() {
            let queue = queue.as_ref().unwrap();
            assert_eq!(queue.next_avail(), 100 + index as u16);
            assert_eq!(queue.next_used(), 70 + index as u16);
        }
        backend.configure(&mut queues, offered(), 2).unwrap();
        let state = fake.0.lock().unwrap();
        assert!(
            state
                .calls
                .iter()
                .any(|call| matches!(call, Call::Setup(0, [100, 101], _)))
        );
        assert!(
            state
                .calls
                .iter()
                .any(|call| matches!(call, Call::Setup(1, [102, 103], _)))
        );
    }

    #[test]
    fn kernel_disables_inactive_taps_but_userspace_drains_all_of_them() {
        for kernel in [true, false] {
            let memory = memory();
            let (mut backend, fake, _, _) = fixture(2, kernel, memory.clone());
            let mut queues = queues(&memory, 2);
            fake.0.lock().unwrap().calls.clear();
            backend.configure(&mut queues, offered(), 1).unwrap();
            if kernel {
                assert!(
                    fake.0
                        .lock()
                        .unwrap()
                        .calls
                        .contains(&Call::Queue(1, false))
                );
                backend.configure(&mut queues, offered(), 2).unwrap();
                assert!(fake.0.lock().unwrap().calls.contains(&Call::Queue(1, true)));
            } else {
                assert!(
                    !fake
                        .0
                        .lock()
                        .unwrap()
                        .calls
                        .iter()
                        .any(|call| matches!(call, Call::Queue(..)))
                );
                assert_eq!(backend.pollfds().len(), 2);
            }
        }
    }

    #[test]
    fn rings_must_fit_one_mapping_and_may_end_at_its_boundary() {
        let memory =
            GuestMemory::from_ranges(&[(GuestAddress(0), 0x1000), (GuestAddress(0x1000), 0x1000)])
                .unwrap();
        let mut config = VringConfigData {
            queue_max_size: 256,
            queue_size: 8,
            desc_table_addr: 0x200,
            avail_ring_addr: 0x300,
            used_ring_addr: 0x400,
            ..Default::default()
        };
        assert!(validate_ring(&memory, &config).is_ok());
        config.desc_table_addr = 0xfc0;
        assert!(validate_ring(&memory, &config).is_err());
        config.desc_table_addr = 0xf80;
        assert!(validate_ring(&memory, &config).is_ok());
        config.used_ring_addr = u64::MAX - 3;
        assert!(validate_ring(&memory, &config).is_err());
    }

    #[test]
    fn memory_table_preserves_aliases_with_distinct_guest_addresses() {
        let mapping = Arc::new(MmapRegion::<()>::new(0x10000).unwrap());
        let memory = GuestMemory::from_regions(vec![
            GuestRegionMmap::with_arc(mapping.clone(), GuestAddress(0)).unwrap(),
            GuestRegionMmap::with_arc(mapping, GuestAddress(0x10000)).unwrap(),
        ])
        .unwrap();
        let (mut backend, fake, _, _) = fixture(1, true, memory.clone());
        backend
            .configure(&mut queues(&memory, 1), offered(), 1)
            .unwrap();
        let state = fake.0.lock().unwrap();
        let regions = state
            .calls
            .iter()
            .find_map(|call| match call {
                Call::Setup(_, _, regions) => Some(regions),
                _ => None,
            })
            .unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!((regions[0].0, regions[1].0), (0, 0x10000));
        assert_eq!(regions[0].2, regions[1].2);
    }

    #[test]
    fn kernel_files_close_while_memory_is_still_alive() {
        let memory = memory();
        let weak = Arc::downgrade(&memory.iter().next().unwrap().get_mmap());
        let (mut backend, fake, _, mut peers) = fixture(1, true, memory.clone());
        backend
            .configure(&mut queues(&memory, 1), offered(), 1)
            .unwrap();
        drop(memory);
        drop(backend);
        assert!(fake.0.lock().unwrap().calls.contains(&Call::Close(0, true)));
        assert!(weak.upgrade().is_none());
        assert_eq!(peers[0].read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn unrecoverable_stop_keeps_memory_mapped_even_if_sender_kept_a_vhost_fd() {
        let memory = memory();
        let weak = Arc::downgrade(&memory.iter().next().unwrap().get_mmap());
        let (mut backend, fake, _, _) = fixture(1, true, memory.clone());
        backend
            .configure(&mut queues(&memory, 1), offered(), 1)
            .unwrap();
        {
            let mut state = fake.0.lock().unwrap();
            state.fail_all_detach = true;
            state.fail_reset = true;
        }
        drop(memory);
        drop(backend);
        assert!(
            weak.upgrade().is_some(),
            "possibly live DMA lost its mapping"
        );
    }

    #[test]
    fn userspace_packet_io_preserves_boundaries_headers_and_queue_ids() {
        let memory = memory();
        let (mut backend, _, peers, _) = fixture(2, false, memory.clone());
        backend
            .configure(&mut queues(&memory, 2), offered(), 1)
            .unwrap();
        let header = Header::default();
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            let mut frame = vec![0; 14];
            frame.extend_from_slice(payload);
            backend.send(1, header, &frame).unwrap();
            let mut packet = [0; 100];
            let count = peers[1].recv(&mut packet).unwrap();
            assert_eq!(&packet[..12], &header.encode());
            assert_eq!(&packet[12..count], &frame);
            peers[1].send(&packet[..count]).unwrap();
            let record = backend.receive(1).unwrap().unwrap();
            let (actual, queue, data) = Header::from_metadata(&record).unwrap();
            assert_eq!((actual, queue, data), (header, 1, frame.as_slice()));
        }
        assert!(backend.receive(1).unwrap().is_none());
    }

    #[test]
    fn userspace_backpressure_arms_pollout_and_never_writes_partial_packets() {
        let memory = memory();
        let (mut backend, _, peers, _) = fixture(1, false, memory.clone());
        backend
            .configure(&mut queues(&memory, 1), offered(), 1)
            .unwrap();
        let frame = vec![0; 1000];
        let mut blocked = false;
        for _ in 0..10000 {
            match backend.send(0, Header::default(), &frame) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    blocked = true;
                    break;
                }
                Err(error) => panic!("packet write: {error}"),
            }
        }
        assert!(blocked);
        assert_ne!(backend.pollfds()[0].1.events & libc::POLLOUT, 0);
        let mut packet = [0; 2048];
        assert_eq!(peers[0].recv(&mut packet).unwrap(), 1012);
        backend.tx_pending(0, false).unwrap();
        assert_eq!(backend.pollfds()[0].1.events & libc::POLLOUT, 0);
    }

    #[test]
    fn kernel_owned_taps_are_not_accessible_through_userspace_io() {
        let (mut backend, _, _, _) = fixture(1, true, memory());
        assert!(backend.receive(0).is_err());
        assert!(backend.send(0, Header::default(), &[0; 14]).is_err());
    }

    #[test]
    fn late_calls_are_drained_after_pause_even_when_no_rings_are_polled() {
        let memory = memory();
        let (mut backend, _, _, _) = fixture(2, true, memory.clone());
        let mut queues = queues(&memory, 2);
        backend.configure(&mut queues, offered(), 2).unwrap();
        backend.pairs[0].call[0].write(1).unwrap();
        backend.pairs[0].call[1].write(7).unwrap();
        backend.pairs[1].call[1].write(2).unwrap();
        backend.pause(&mut queues).unwrap();
        assert!(backend.pollfds().is_empty());
        assert_eq!(backend.drain_calls().unwrap(), vec![0, 1, 3]);
        assert!(backend.drain_calls().unwrap().is_empty());
    }

    #[test]
    fn oversized_tap_packet_is_rejected_instead_of_forwarding_a_truncated_frame() {
        let memory = memory();
        let (mut backend, _, peers, _) = fixture(1, false, memory.clone());
        backend
            .configure(&mut queues(&memory, 1), offered(), 1)
            .unwrap();
        let mut packet = Header::default().encode().to_vec();
        packet.resize(12 + MAX_FRAME_LEN + 20, 0);
        peers[0].send(&packet).unwrap();
        let error = backend.receive(0).unwrap_err();
        assert!(error.to_string().contains("oversized TAP packet"));
    }

    #[test]
    fn ordinary_packet_socket_is_rejected_by_real_tap_validation_and_closed() {
        let (socket, peer) = UnixDatagram::pair().unwrap();
        assert!(TapBackend::new(vec![socket.into()], 1, false, memory(), offered()).is_err());
        assert!(peer.send(b"closed").is_err());
    }

    #[test]
    fn real_vhost_setup_accepts_only_shared_ring_features_without_booting_a_vm() {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vhost-net")
        {
            Ok(file) => file,
            Err(error) => {
                eprintln!("skipping vhost ioctl check: /dev/vhost-net: {error}");
                return;
            }
        };
        let memory = memory();
        let queues = queues(&memory, 1);
        let kick = [
            EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC).unwrap(),
            EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC).unwrap(),
        ];
        let call = [
            EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC).unwrap(),
            EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC).unwrap(),
        ];
        let regions = memory
            .iter()
            .map(|region| VhostUserMemoryRegionInfo {
                guest_phys_addr: region.start_addr().0,
                memory_size: region.len(),
                userspace_addr: memory.get_host_address(region.start_addr()).unwrap() as u64,
                mmap_handle: -1,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Syscalls.owner(&file, &memory).unwrap();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    Syscalls
                        .setup(
                            &file,
                            KernelSetup {
                                memory: &memory,
                                features: offered(),
                                regions: &regions,
                                rings: [queues[0].as_ref().unwrap(), queues[1].as_ref().unwrap()],
                                kick: &kick,
                                call: &call,
                            },
                        )
                        .unwrap()
                })
                .join()
                .unwrap()
        });
        assert_eq!(Syscalls.base(&file, &memory, 0).unwrap(), 3);
        assert_eq!(Syscalls.base(&file, &memory, 1).unwrap(), 4);
        Syscalls.reset_owner(&file, &memory).unwrap();
        drop(file);
        for fd in &kick {
            fd.write(1).unwrap();
            assert_eq!(fd.read().unwrap(), 1);
        }
    }
}
