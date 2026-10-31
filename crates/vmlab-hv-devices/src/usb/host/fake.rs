// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! A fake usbdevfs kernel side, for tests.
//!
//! Simulates a two-interface USB 2.0 high-speed device — a bulk loopback
//! pair and an isochronous IN sink — behind the same [`UsbFs`] trait as the
//! real ioctls. The fake records claim, release and connect calls so tests
//! can assert the host-driver hand-off, and the test plays the device's
//! role by calling [`FakeUsbFs::run_irq`] between polls.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io;
use std::os::raw::c_int;
use std::sync::Arc;

use parking_lot::Mutex;

use super::fs::UsbFs;
use super::sys;

/// Everything the fake remembers, for assertions.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct FakeCalls {
    pub claims: Vec<u8>,
    pub releases: Vec<u8>,
    pub connects: Vec<u8>,
    pub set_interfaces: Vec<(u8, u8)>,
    pub set_configurations: Vec<u8>,
    pub cleared_halts: Vec<u8>,
}

/// Device descriptor blob the fake serves from the usbdevfs file: one
/// configuration, two interfaces (bulk in/out, isochronous in), plus an
/// alt setting on the bulk interface.
pub const FAKE_DESCRIPTORS: &[u8] = &[
    0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x42, 0x42, 0x66, 0x66, 0x01, 0x00, 0x01, 0x02,
    0x00, 0x01, // Config 1: two interfaces.
    0x09, 0x02, 0x46, 0x00, 0x02, 0x01, 0x00, 0x80,
    0x32, // Interface 0 alt 0: bulk in + bulk out.
    0x09, 0x04, 0x00, 0x00, 0x02, 0xff, 0x00, 0x00, 0x00, 0x07, 0x05, 0x81, 0x02, 0x40, 0x00, 0x00,
    0x07, 0x05, 0x02, 0x02, 0x40, 0x00, 0x00, // Interface 1 alt 0: iso in, 512-byte packets.
    0x09, 0x04, 0x01, 0x00, 0x01, 0xff, 0x01, 0x00, 0x00, 0x07, 0x05, 0x83, 0x01, 0x00, 0x02,
    0x01, // Interface 0 alt 1: the same bulk pair.
    0x09, 0x04, 0x00, 0x01, 0x02, 0xff, 0x00, 0x00, 0x00, 0x07, 0x05, 0x81, 0x02, 0x40, 0x00, 0x00,
    0x07, 0x05, 0x02, 0x02, 0x40, 0x00, 0x00,
];

/// How the fake completes one submitted URB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    /// Complete successfully, delivering `data` (IN) or reporting that many
    /// bytes sent (OUT; `data` may be empty).
    Data(Vec<u8>),
    /// Complete with -EPIPE.
    Stall,
    /// Stay in flight until discarded, released or unplugged.
    Hang,
}

/// One URB the fake holds, as the kernel would: the URB address is only
/// valid while the owning Transfer is alive (submitted and not reaped).
struct Submitted {
    tag: usize,
    endpoint: u8,
    urb: *mut sys::usbdevfs_urb,
    /// The transfer buffer's address and length, for IN deliveries.
    buffer: *mut u8,
    buffer_len: usize,
    /// The payload at submission time (OUT payloads).
    submitted: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    calls: FakeCalls,
    /// Completions queued for the next URBs `run_irq` services; missing
    /// entries mean an empty `Data`.
    completions: VecDeque<Completion>,
    /// Tags ready to reap.
    completed: VecDeque<usize>,
    lost: bool,
    claim_fails: Vec<u8>,
}

/// The fake usbdevfs. Cloning shares the same fake device, so a test can
/// keep a handle after boxing one clone for the device layer.
#[derive(Clone)]
pub struct FakeUsbFs {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Mutex<Shared>,
    /// In flight and finished-but-unreaped URBs.
    in_flight: Mutex<Vec<Submitted>>,
    done: Mutex<Vec<Submitted>>,
    next_tag: std::sync::atomic::AtomicUsize,
}

// SAFETY: the raw pointers in `Submitted` are only dereferenced between
// submit and reap from the thread that owns the device layer, the same
// ownership the real usbdevfs ABI gives the kernel.
unsafe impl Send for Inner {}
// SAFETY: as above; the fake never dereferences the pointers from another
// thread than the one that submitted.
unsafe impl Sync for Inner {}

impl FakeUsbFs {
    pub fn new() -> Self {
        FakeUsbFs {
            inner: Arc::new(Inner {
                shared: Mutex::new(Shared::default()),
                in_flight: Mutex::new(Vec::new()),
                done: Mutex::new(Vec::new()),
                next_tag: std::sync::atomic::AtomicUsize::new(1),
            }),
        }
    }

    fn shared(&self) -> &Mutex<Shared> {
        &self.inner.shared
    }

    /// Make `claim_interface(interface)` fail with EBUSY.
    pub fn fail_claim(&self, interface: u8) {
        self.shared().lock().claim_fails.push(interface);
    }

    /// Queue the completion the next URB `run_irq` services receives.
    pub fn complete_with(&self, completion: Completion) {
        self.shared().lock().completions.push_back(completion);
    }

    /// Complete every in-flight URB, consuming queued completions (empty
    /// `Data` when none is queued). This is the fake device's IRQ.
    pub fn run_irq(&self) {
        // Drained first: a Hang goes back into the list while the loop
        // still needs to finish the rest.
        let pending: Vec<Submitted> = self.inner.in_flight.lock().drain(..).collect();
        for urb in pending {
            let completion = self
                .shared()
                .lock()
                .completions
                .pop_front()
                .unwrap_or(Completion::Data(Vec::new()));
            match completion {
                Completion::Hang => self.inner.in_flight.lock().push(urb),
                Completion::Stall => self.finish(&urb, -libc::EPIPE, &[]),
                Completion::Data(data) => {
                    let actual = data.len().min(urb.buffer_len);
                    // SAFETY: the buffer belongs to the Transfer that
                    // submitted this URB and stays alive until reaped.
                    unsafe {
                        if actual > 0 {
                            std::ptr::copy_nonoverlapping(data.as_ptr(), urb.buffer, actual);
                        }
                    }
                    self.finish(&urb, 0, &data[..actual]);
                }
            }
        }
    }

    /// Write a URB result in place, as the kernel does at completion, and
    /// mark it reapable.
    fn finish(&self, urb: &Submitted, status: i32, data: &[u8]) {
        // SAFETY: the URB belongs to the Transfer that submitted it and
        // stays alive until reaped; only kernel-owned result fields are
        // written.
        unsafe {
            (*urb.urb).status = status;
            (*urb.urb).actual_length = data.len() as i32;
            let packets = (*urb.urb).number_of_packets_or_stream_id;
            if packets > 0 && status == 0 {
                // Isochronous: split the payload across the packet
                // descriptors the URB carried.
                let iso = (*urb.urb).iso_frame_desc.as_mut_ptr();
                let mut offset = 0usize;
                for i in 0..packets as usize {
                    let length = (*iso.add(i)).length as usize;
                    let got = data.len().saturating_sub(offset).min(length);
                    (*iso.add(i)).actual_length = got as u32;
                    (*iso.add(i)).status = 0;
                    offset += got;
                }
            }
        }
        self.shared().lock().completed.push_back(urb.tag);
        self.inner.done.lock().push(Submitted {
            tag: urb.tag,
            endpoint: urb.endpoint,
            urb: urb.urb,
            buffer: urb.buffer,
            buffer_len: urb.buffer_len,
            submitted: urb.submitted.clone(),
        });
    }

    /// Simulate the device being unplugged: in-flight URBs complete with
    /// -ENODEV and every later call fails with ENODEV.
    pub fn unplug(&self) {
        self.shared().lock().lost = true;
        // The guard is dropped before finish() takes the same lock again.
        for urb in self.inner.in_flight.lock().drain(..) {
            self.finish(&urb, -libc::ENODEV, &[]);
        }
    }

    /// OUT payloads that reached the "device", in submission order.
    pub fn submitted_payloads(&self) -> Vec<(u8, Vec<u8>)> {
        self.inner
            .in_flight
            .lock()
            .iter()
            .map(|u| (u.endpoint, u.submitted.clone()))
            .collect()
    }

    /// The recorded claim/release/connect history.
    pub fn calls(&self) -> FakeCalls {
        self.shared().lock().calls.clone()
    }

    fn errno(error: i32) -> io::Error {
        io::Error::from_raw_os_error(error)
    }
}

impl Default for FakeUsbFs {
    fn default() -> Self {
        Self::new()
    }
}

impl UsbFs for FakeUsbFs {
    fn read_descriptors(&self) -> io::Result<Vec<u8>> {
        Ok(FAKE_DESCRIPTORS.to_vec())
    }

    fn submit_urb(&self, urb: *mut sys::usbdevfs_urb) -> io::Result<()> {
        let shared = self.shared().lock();
        if shared.lost {
            return Err(Self::errno(libc::ENODEV));
        }
        drop(shared);
        let tag = self
            .inner
            .next_tag
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: the URB is owned by a Transfer held in the device layer's
        // in-flight map until it is reaped or dropped, so reading through
        // this pointer mirrors what the kernel does at submission.
        unsafe {
            (*urb).usercontext = tag;
            let buffer_len = (*urb).buffer_length.max(0) as usize;
            let (buffer, submitted) = if (*urb).buffer.is_null() || buffer_len == 0 {
                (std::ptr::null_mut(), Vec::new())
            } else {
                (
                    (*urb).buffer as *mut u8,
                    std::slice::from_raw_parts((*urb).buffer as *const u8, buffer_len).to_vec(),
                )
            };
            self.inner.in_flight.lock().push(Submitted {
                tag,
                endpoint: (*urb).endpoint,
                urb,
                buffer,
                buffer_len,
                submitted,
            });
        }
        Ok(())
    }

    fn reap_urb(&self) -> io::Result<*mut sys::usbdevfs_urb> {
        let mut shared = self.shared().lock();
        if shared.lost {
            return Err(Self::errno(libc::ENODEV));
        }
        let Some(tag) = shared.completed.pop_front() else {
            return Err(Self::errno(libc::EAGAIN));
        };
        let mut done = self.inner.done.lock();
        match done.iter().position(|u| u.tag == tag) {
            Some(index) => Ok(done.remove(index).urb),
            None => Err(Self::errno(libc::EAGAIN)),
        }
    }

    fn discard_urb(&self, urb: *mut sys::usbdevfs_urb) -> io::Result<()> {
        // SAFETY: the URB is alive as long as its Transfer is in flight.
        let tag = unsafe { (*urb).usercontext };
        // The kernel unlinks the URB and still reaps it, with -ENOENT.
        // The index is found under one lock guard and removed under
        // another: re-entering the same mutex would deadlock.
        let index = self
            .inner
            .in_flight
            .lock()
            .iter()
            .position(|u| u.tag == tag);
        if let Some(index) = index {
            let urb = self.inner.in_flight.lock().remove(index);
            self.finish(&urb, -libc::ENOENT, &[]);
        }
        Ok(())
    }

    fn control(&self, transfer: &sys::usbdevfs_ctrltransfer) -> io::Result<c_int> {
        let shared = self.shared().lock();
        if shared.lost {
            return Err(Self::errno(libc::ENODEV));
        }
        // The fake answers GET_CONFIGURATION with configuration 1.
        if transfer.bRequest == 8 {
            // SAFETY: a one-byte write into the caller's buffer.
            unsafe {
                *(transfer.data as *mut u8) = 1;
            }
            return Ok(1);
        }
        Ok(0)
    }

    fn claim_interface(&self, interface: u8) -> io::Result<()> {
        let mut shared = self.shared().lock();
        if shared.lost {
            return Err(Self::errno(libc::ENODEV));
        }
        if shared.claim_fails.contains(&interface) {
            return Err(Self::errno(libc::EBUSY));
        }
        shared.calls.claims.push(interface);
        Ok(())
    }

    fn release_interface(&self, interface: u8) -> io::Result<()> {
        self.shared().lock().calls.releases.push(interface);
        Ok(())
    }

    fn connect_interface(&self, interface: u8) -> io::Result<()> {
        self.shared().lock().calls.connects.push(interface);
        Ok(())
    }

    fn set_interface(&self, interface: u8, alt_setting: u8) -> io::Result<()> {
        self.shared()
            .lock()
            .calls
            .set_interfaces
            .push((interface, alt_setting));
        Ok(())
    }

    fn set_configuration(&self, config: u8) -> io::Result<()> {
        self.shared().lock().calls.set_configurations.push(config);
        Ok(())
    }

    fn clear_halt(&self, endpoint: u8) -> io::Result<()> {
        self.shared().lock().calls.cleared_halts.push(endpoint);
        Ok(())
    }

    fn get_speed(&self) -> io::Result<c_int> {
        Ok(3) // high speed
    }
}
