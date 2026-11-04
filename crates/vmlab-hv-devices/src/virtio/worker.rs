//! Queue service on a worker thread, with a snapshot barrier.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;

use parking_lot::Mutex;
use virtio_queue::Queue;

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

enum Command {
    Kick,
    Barrier(mpsc::SyncSender<()>),
    Quiesce(mpsc::SyncSender<()>),
    Resume,
    Stop,
}

struct Pending {
    queues: Vec<AtomicBool>,
    scheduled: AtomicBool,
}

impl Pending {
    fn drain(&self, device: &Mutex<Box<dyn VirtioDevice>>) {
        for (index, pending) in self.queues.iter().enumerate() {
            if pending.swap(false, Ordering::AcqRel) {
                device.lock().queue_notify(index as u16);
            }
        }
    }
}

/// Adapt a device whose queue service can block to non-blocking vCPU kicks.
/// Configuration and snapshot bytes remain those of the wrapped device.
/// Device type, offered features, config length, and queue limits must be fixed.
pub struct QueueWorker {
    device: Arc<Mutex<Box<dyn VirtioDevice>>>,
    sizes: Vec<u16>,
    kind: u32,
    features: u64,
    config_len: u64,
    pending: Arc<Pending>,
    sender: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl QueueWorker {
    pub fn new(device: Box<dyn VirtioDevice>) -> Result<Self, DeviceError> {
        // Capability reads can hold the transport's interrupt lock. Cache
        // them so they never wait for a queue issuing an interrupt.
        let sizes = device.queue_max_sizes().to_vec();
        let kind = device.device_type();
        let features = device.features();
        let config_len = device.config_len();
        let name = format!("{}-queues", device.state_id());
        let device = Arc::new(Mutex::new(device));
        let pending = Arc::new(Pending {
            queues: sizes.iter().map(|_| AtomicBool::new(false)).collect(),
            scheduled: AtomicBool::new(false),
        });
        let (sender, receiver) = mpsc::channel();
        let work = pending.clone();
        let inner = device.clone();
        let worker = std::thread::Builder::new().name(name).spawn(move || {
            let mut paused = false;
            while let Ok(command) = receiver.recv() {
                match command {
                    Command::Kick => {
                        work.scheduled.store(false, Ordering::Release);
                        if !paused {
                            work.drain(&inner);
                        }
                    }
                    Command::Barrier(done) => {
                        let _ = done.send(());
                    }
                    Command::Quiesce(done) => {
                        paused = true;
                        inner.lock().quiesce();
                        let _ = done.send(());
                    }
                    Command::Resume => {
                        inner.lock().resume_snapshot();
                        paused = false;
                        work.drain(&inner);
                    }
                    Command::Stop => break,
                }
            }
        })?;
        Ok(Self {
            device,
            sizes,
            kind,
            features,
            config_len,
            pending,
            sender,
            worker: Some(worker),
        })
    }

    fn barrier(&self, quiesce: bool) {
        let (done, wait) = mpsc::sync_channel(0);
        let command = if quiesce {
            Command::Quiesce(done)
        } else {
            Command::Barrier(done)
        };
        self.sender
            .send(command)
            .expect("virtio queue worker is alive");
        wait.recv()
            .expect("virtio queue worker completed its barrier");
    }
}

impl Drop for QueueWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl VirtioDevice for QueueWorker {
    fn device_type(&self) -> u32 {
        self.kind
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }
    fn features(&self) -> u64 {
        self.features
    }
    fn ack_features(&mut self, features: u64) {
        self.device.lock().ack_features(features);
    }
    fn config_len(&self) -> u64 {
        self.config_len
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.device.lock().read_config(offset, data);
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        self.device.lock().write_config(offset, data);
    }
    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        self.device.lock().activate(mem, interrupt, queues)
    }
    fn queue_notify(&mut self, index: u16) {
        let Some(queue) = self.pending.queues.get(index as usize) else {
            return;
        };
        queue.store(true, Ordering::Release);
        if !self.pending.scheduled.swap(true, Ordering::AcqRel) {
            self.sender
                .send(Command::Kick)
                .expect("virtio queue worker is alive");
        }
    }
    fn reset(&mut self) {
        self.barrier(false);
        for queue in &self.pending.queues {
            queue.store(false, Ordering::Release);
        }
        self.device.lock().reset();
    }
}

impl Snapshot for QueueWorker {
    fn quiesce(&mut self) {
        self.barrier(true);
    }
    fn resume_snapshot(&mut self) {
        self.sender
            .send(Command::Resume)
            .expect("virtio queue worker is alive");
    }
    fn state_id(&self) -> String {
        self.device.lock().state_id()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.device.lock().save_state()
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.device.lock().restore_state(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::{block::Block, testing::MemDisk};
    use std::time::Duration;

    struct Slow {
        inner: Block,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Snapshot for Slow {
        fn state_id(&self) -> String {
            self.inner.state_id()
        }
        fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
            self.inner.save_state()
        }
        fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
            self.inner.restore_state(state)
        }
    }
    impl VirtioDevice for Slow {
        fn device_type(&self) -> u32 {
            self.inner.device_type()
        }
        fn queue_max_sizes(&self) -> &[u16] {
            self.inner.queue_max_sizes()
        }
        fn features(&self) -> u64 {
            self.inner.features()
        }
        fn ack_features(&mut self, features: u64) {
            self.inner.ack_features(features);
        }
        fn config_len(&self) -> u64 {
            self.inner.config_len()
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            self.inner.read_config(offset, data);
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            self.inner.write_config(offset, data);
        }
        fn activate(
            &mut self,
            mem: GuestMemory,
            interrupt: Arc<dyn VirtioInterrupt>,
            queues: Vec<Queue>,
        ) -> Result<(), DeviceError> {
            self.inner.activate(mem, interrupt, queues)
        }
        fn queue_notify(&mut self, _: u16) {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            self.completed.fetch_add(1, Ordering::SeqCst);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
    }

    #[test]
    fn kicks_do_not_wait_for_io_and_quiesce_waits_for_completion() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let block = Block::new("test", Box::new(MemDisk::new(4096)));
        let expected = block.save_state().unwrap();
        let capabilities = (block.device_type(), block.features(), block.config_len());
        let sizes = block.queue_max_sizes().to_vec();
        let mut device = QueueWorker::new(Box::new(Slow {
            inner: block,
            entered,
            release: released,
            completed: completed.clone(),
        }))
        .unwrap();
        device.queue_notify(0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert_eq!(
            (device.device_type(), device.features(), device.config_len()),
            capabilities,
            "transport capability reads must not wait for blocked queue I/O"
        );
        assert_eq!(device.queue_max_sizes(), sizes);
        let (quiesced, parked) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            device.quiesce();
            quiesced
                .send(device)
                .unwrap_or_else(|_| panic!("quiesce receiver is alive"));
        });
        assert!(parked.recv_timeout(Duration::from_millis(20)).is_err());
        release.send(()).unwrap();
        let mut device = parked.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.join().unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(device.save_state().unwrap(), expected);
        device.queue_notify(0);
        assert!(started.recv_timeout(Duration::from_millis(20)).is_err());
        device.resume_snapshot();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        release.send(()).unwrap();
        device.quiesce();
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        device.restore_state(&expected).unwrap();
        assert_eq!(device.save_state().unwrap(), expected);
        device.reset();
    }

    #[test]
    fn paused_kicks_are_coalesced_without_losing_queues() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = Block::new("test", Box::new(MemDisk::new(4096)))
            .with_num_queues(4)
            .unwrap();
        let mut device = QueueWorker::new(Box::new(Slow {
            inner,
            entered,
            release: released,
            completed: completed.clone(),
        }))
        .unwrap();
        device.quiesce();
        for _ in 0..1000 {
            for index in [0, 2, 3] {
                device.queue_notify(index);
            }
        }
        device.queue_notify(4);
        assert!(started.recv_timeout(Duration::from_millis(20)).is_err());
        device.resume_snapshot();
        for _ in 0..3 {
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            release.send(()).unwrap();
        }
        device.quiesce();
        assert_eq!(completed.load(Ordering::SeqCst), 3);
    }
}
