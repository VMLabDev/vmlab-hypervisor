#![allow(dead_code)]
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use vmlab_hv_display::{
    Capabilities, Client, DisplayServer, Frame, FrameSource, InputSink, Message, Pixels, Rect,
};

pub struct Source(pub Mutex<Frame>);
impl FrameSource for Source {
    fn frame(&self) -> io::Result<Frame> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn readback(&self, frame: &Frame) -> io::Result<Vec<u8>> {
        Ok(vec![42; frame.stride as usize * frame.height as usize])
    }
}
#[derive(Default)]
pub struct Sink(pub Mutex<Vec<Message>>);
impl InputSink for Sink {
    fn input(&self, message: Message) {
        self.0.lock().unwrap().push(message);
    }
}
pub fn frame(value: u8, width: u32, height: u32) -> Frame {
    Frame {
        flags: Default::default(),
        generation: 0,
        width,
        height,
        stride: width * 4,
        drm_fourcc: vmlab_hv_display::protocol::XRGB8888,
        modifier: 0,
        timestamp_ns: value as u64,
        damage: vec![Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }],
        pixels: Pixels::Shm(Arc::new(vec![value; (width * height * 4) as usize])),
    }
}
pub fn server() -> (DisplayServer, Arc<Source>, Arc<Sink>) {
    let source = Arc::new(Source(Mutex::new(frame(1, 4, 3))));
    let sink = Arc::new(Sink::default());
    (
        DisplayServer::new(source.clone(), sink.clone()).unwrap(),
        source,
        sink,
    )
}
pub fn socket() -> (UnixStream, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    }
    (a, b)
}
pub fn client(server: &DisplayServer, caps: u32) -> (Client, JoinHandle<io::Result<()>>) {
    let (a, b) = socket();
    let s = server.clone();
    let handle = std::thread::spawn(move || s.serve_native(a));
    (Client::from_stream(b, Capabilities(caps)).unwrap(), handle)
}
pub fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(t) = f() {
            return t;
        }
        assert!(std::time::Instant::now() < deadline, "condition timed out");
        std::thread::park_timeout(Duration::from_millis(2));
    }
}
