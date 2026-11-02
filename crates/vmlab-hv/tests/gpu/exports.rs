use super::*;

#[derive(Default)]
struct Frames {
    surface: u64,
    configures: usize,
    buffers: Vec<std::os::fd::OwnedFd>,
    stride: u32,
    kind: Option<SurfaceKind>,
    last_buffer: u32,
    last_seq: u64,
    modifier: u64,
}

impl Frames {
    fn next(&mut self, client: &DisplayClient) -> (Vec<vmlab_hv_display::Rect>, u64) {
        loop {
            let packet = client.receive().unwrap();
            match packet.message {
                Message::SurfaceConfigure {
                    surface_id,
                    stride,
                    kind,
                    modifier,
                    ..
                } => {
                    self.surface = surface_id;
                    self.stride = stride;
                    self.kind = Some(kind);
                    self.modifier = modifier;
                    self.buffers = packet.fds;
                    self.configures += 1;
                }
                Message::FrameReady {
                    surface_id,
                    buffer,
                    seq,
                    timestamp_ns,
                    damage,
                    ..
                } => {
                    assert_eq!(surface_id, self.surface);
                    self.last_buffer = buffer;
                    self.last_seq = seq;
                    return (damage, monotonic_ns().saturating_sub(timestamp_ns));
                }
                Message::LedState { .. } => {}
                message => panic!("unexpected frame message {message:?}"),
            }
        }
    }

    fn release(&self, client: &DisplayClient) {
        client
            .release(self.surface, self.last_buffer, self.last_seq)
            .unwrap();
    }

    fn pixel(&self) -> [u8; 4] {
        let mut pixel = [0; 4];
        File::from(self.buffers[self.last_buffer as usize].try_clone().unwrap())
            .read_exact_at(&mut pixel, 300 * self.stride as u64 + 400 * 4)
            .unwrap();
        pixel
    }
}

fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_MONOTONIC is valid and time is writable.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    assert_eq!(result, 0);
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

fn export_stats(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path.join("hv.log"))
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| {
            record["message"]
                .as_str()?
                .strip_prefix("scanout_export ")
                .and_then(|message| serde_json::from_str(message).ok())
        })
        .next_back()
        .expect("renderer export statistics")
}

fn colour(frame: &Frames, expected: [u8; 3]) {
    let pixel = frame.pixel();
    assert!(
        pixel[..3]
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.abs_diff(expected) <= 2),
        "pixel={pixel:?}, expected={expected:?}"
    );
}

fn request(agent: &mut AgentClient, id: u32, colour: Option<[f32; 3]>) {
    let mut request = json!({"id": id, "damage": [16, 24, 32, 40]});
    if let Some(colour) = colour {
        request["colour"] = json!(colour);
    }
    let output = agent.exec(&["python3", "-c", "import os,sys; open('/run/gpu-request.tmp','w').write(sys.argv[1]); os.rename('/run/gpu-request.tmp','/run/gpu-request.json')", &request.to_string()], Duration::from_secs(10)).unwrap();
    assert_eq!(
        output.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn exercise(path: &Path, agent: &mut AgentClient) {
    let dma = DisplayClient::connect(
        path.join("display.sock"),
        Capabilities(
            Capabilities::FRAMES
                | Capabilities::DMABUF_OK
                | Capabilities::STABLE_BUFFERS
                | Capabilities::INPUT,
        ),
    )
    .unwrap();
    dma.stream()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut gpu = Frames::default();
    gpu.next(&dma);
    assert_eq!(gpu.kind, Some(SurfaceKind::Dmabuf));
    let implicit = std::env::var("VMLAB_HV_TEST_SCANOUT").is_ok_and(|mode| mode == "implicit");
    assert_eq!(gpu.buffers.len(), if implicit { 3 } else { 1 });
    let modifier = gpu.modifier;
    let surface = gpu.surface;
    gpu.release(&dma);
    let baseline = export_stats(path);
    let mut dma_latency = Vec::new();
    let expected = vec![vmlab_hv_display::Rect {
        // This guest kernel expands legacy DIRTYFB damage to the whole plane.
        // The host must preserve the RESOURCE_FLUSH rectangle it actually sends.
        x: 0,
        y: 0,
        width: 1024,
        height: 768,
    }];
    for id in 1..=8 {
        let (damage, ns) = std::thread::scope(|scope| {
            let frame = scope.spawn(|| gpu.next(&dma));
            request(agent, id, None);
            frame.join().unwrap()
        });
        assert_eq!(
            damage, expected,
            "flush damage must reach the client unchanged"
        );
        assert_eq!(gpu.surface, surface);
        assert_eq!(gpu.configures, 1, "import cache survives every flush");
        dma_latency.push(ns);
        gpu.release(&dma);
    }
    let before_cpu = export_stats(path);
    assert_eq!(
        before_cpu["readbacks"], 0,
        "dma-only export read pixels on the CPU"
    );

    let cpu = DisplayClient::connect(
        path.join("display.sock"),
        Capabilities(Capabilities::FRAMES),
    )
    .unwrap();
    cpu.stream()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut shm = Frames::default();
    shm.next(&cpu);
    assert_eq!(shm.kind, Some(SurfaceKind::Shm));
    colour(&shm, [26, 51, 178]);
    shm.release(&cpu);
    let mut cpu_latency = Vec::new();
    for id in 9..=16 {
        let ((damage, ns), (cpu_damage, cpu_ns)) = std::thread::scope(|scope| {
            let dma_frame = scope.spawn(|| gpu.next(&dma));
            let cpu_frame = scope.spawn(|| shm.next(&cpu));
            request(agent, id, None);
            (dma_frame.join().unwrap(), cpu_frame.join().unwrap())
        });
        assert_eq!(damage, expected);
        dma_latency.push(ns);
        gpu.release(&dma);
        assert_eq!(cpu_damage, expected);
        cpu_latency.push(cpu_ns);
        colour(&shm, [26, 51, 178]);
        shm.release(&cpu);
    }
    assert_eq!(gpu.configures, 1);
    assert_eq!(shm.configures, 1);
    let pooled = gpu.buffers.len() == 3;
    drop(dma);
    let dma = DisplayClient::connect(
        path.join("display.sock"),
        Capabilities(
            Capabilities::FRAMES
                | Capabilities::DMABUF_OK
                | Capabilities::STABLE_BUFFERS
                | Capabilities::INPUT,
        ),
    )
    .unwrap();
    assert!(dma.capabilities.has(Capabilities::INPUT));
    dma.stream()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut gpu = Frames::default();
    gpu.next(&dma);
    if !pooled {
        let waiting = || {
            std::fs::read_to_string(path.join("hv.log"))
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|record| record["message"] == "scanout_wait_for_release")
                .count()
        };
        let before = waiting();
        std::thread::scope(|scope| {
            let clear = scope.spawn(|| request(agent, 18, Some([0.3, 0.4, 0.5])));
            let deadline = Instant::now() + Duration::from_secs(5);
            while waiting() == before {
                assert!(
                    Instant::now() < deadline,
                    "guest did not wait for the held direct buffer"
                );
                std::thread::park_timeout(Duration::from_millis(2));
            }
            colour(&shm, [26, 51, 178]);
            dma.send(&Message::RequestSize {
                width: 1280,
                height: 800,
            })
            .unwrap();
            gpu.release(&dma);
            clear.join().unwrap();
        });
    } else {
        assert_eq!(gpu.buffers.len(), 3);
        let mut held = vec![(gpu.surface, gpu.last_buffer, gpu.last_seq)];
        for id in 17..=18 {
            request(agent, id, None);
            gpu.next(&dma);
            held.push((gpu.surface, gpu.last_buffer, gpu.last_seq));
            shm.next(&cpu);
            shm.release(&cpu);
        }
        assert_eq!(
            held.iter()
                .map(|frame| frame.1)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        request(agent, 20, Some([0.3, 0.4, 0.5]));
        std::thread::park_timeout(Duration::from_millis(20));
        let first = held.remove(0);
        dma.release(first.0, first.1, first.2).unwrap();
        // No new guest flush: freeing a slot must deliver the skipped latest frame.
        gpu.next(&dma);
        gpu.release(&dma);
        for (surface, buffer, seq) in held {
            dma.release(surface, buffer, seq).unwrap();
        }
    }
    drop(dma);
    loop {
        shm.next(&cpu);
        let updated = shm.pixel()[..3]
            .iter()
            .zip([128u8, 102, 76])
            .all(|(actual, expected)| actual.abs_diff(expected) <= 2);
        shm.release(&cpu);
        if updated {
            break;
        }
    }
    request(agent, 21, Some([0.7, 0.2, 0.1]));
    loop {
        shm.next(&cpu);
        let restored = shm.pixel()[..3]
            .iter()
            .zip([26u8, 51, 178])
            .all(|(actual, expected)| actual.abs_diff(expected) <= 2);
        shm.release(&cpu);
        if restored {
            break;
        }
    }
    let stats = export_stats(path);
    eprintln!(
        "S1_export {}",
        json!({ "baseline": baseline, "stats": stats, "pooled": pooled, "modifier": modifier,
        "dma_flush_to_client_ns": dma_latency, "shm_flush_to_client_ns": cpu_latency })
    );
}
