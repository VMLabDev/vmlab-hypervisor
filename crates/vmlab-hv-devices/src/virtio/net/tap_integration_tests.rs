fn tap_worker_fixture(
    pairs: u16,
    mergeable: bool,
    kernel: bool,
) -> (Fixture, Vec<std::os::unix::net::UnixDatagram>) {
    let mut f = Fixture::new_mode(Some(pairs));
    f.accepted = Some(f.take_switch_side());
    f.dev.quiesce();
    if !mergeable {
        f.dev
            .ack_features(f.dev.features() & !F_EVENT_IDX & !F_MRG_RXBUF);
    }
    let (backend, peers) = tap::test_backend(pairs, kernel, f.mem.clone());
    {
        let mut shared = lock(&f.dev.shared);
        shared.tap = Some(backend);
        shared.tap_mode = true;
    }
    f.dev.resume_snapshot();
    (f, peers)
}

fn tap_worker_queue_tx(f: &mut Fixture, pair: u16, header: Header, frame: &[u8], slot: u64) {
    let address = f.data(slot);
    let mut body = header.encode().to_vec();
    body.extend_from_slice(frame);
    f.mem.write_slice(&body, GuestAddress(address)).unwrap();
    f.vqs[usize::from(2 * pair + 1)].add_chain(&f.mem, &[(address, body.len() as u32, false)]);
    f.dev.queue_notify(2 * pair + 1);
}

fn tap_worker_set_pairs(f: &mut Fixture, pairs: u16) -> u8 {
    let index = f.vqs.len() - 1;
    let before = f.vqs[index].used_idx(&f.mem);
    let command = f.data(8);
    let reply = f.data(9);
    f.mem
        .write_slice(
            &[4, 0, pairs as u8, (pairs >> 8) as u8],
            GuestAddress(command),
        )
        .unwrap();
    f.mem.write_obj(0xabu8, GuestAddress(reply)).unwrap();
    f.vqs[index].add_chain(&f.mem, &[(command, 4, false), (reply, 1, true)]);
    f.dev.queue_notify(index as u16);
    wait_for("TAP MQ control completion", || {
        f.vqs[index].used_idx(&f.mem) == before + 1
    });
    f.mem.read_obj(GuestAddress(reply)).unwrap()
}

fn tap_worker_receive(peer: &std::os::unix::net::UnixDatagram) -> Vec<u8> {
    let mut body = vec![0; MAX_FRAME + 12];
    let count = peer.recv(&mut body).unwrap();
    body.truncate(count);
    body
}

#[test]
fn tap_worker_transmit_preserves_gso_and_queue_identity() {
    let (mut f, peers) = tap_worker_fixture(2, false, false);
    assert_eq!(tap_worker_set_pairs(&mut f, 2), 0);
    let (header, mut frame) = gso_packet(true, 2200);
    for pair in 0..2 {
        frame[35] = pair as u8 + 1;
        tap_worker_queue_tx(&mut f, pair, header, &frame, u64::from(pair));
        let received = tap_worker_receive(&peers[usize::from(pair)]);
        assert_eq!(&received[..12], &header.encode());
        assert_eq!(&received[12..], &frame);
        wait_for("TAP TX used entry", || {
            f.vqs[usize::from(2 * pair + 1)].used_idx(&f.mem) == 1
        });
        assert_eq!(
            f.vqs[usize::from(2 * pair + 1)].used_entry(&f.mem, 0).1,
            (12 + frame.len()) as u32
        );
    }
    wait_for("TAP queue-specific TX interrupts", || {
        let irqs = f.irq.queues.lock();
        irqs.contains(&1) && irqs.contains(&3)
    });
    assert!(lock(&f.dev.shared).tap_error.is_none());
}

#[test]
fn tap_worker_receive_handles_nonmerge_and_merge_buffers() {
    for mergeable in [false, true] {
        let (mut f, peers) = tap_worker_fixture(1, mergeable, false);
        let frame = checksum_frame(false);
        let first = f.data(0);
        let second = f.data(1);
        let first_capacity = if mergeable { 25 } else { 12 + frame.len() };
        f.vqs[0].add_chain(
            &f.mem,
            &[
                (first, 5, true),
                (first + 5, (first_capacity - 5) as u32, true),
            ],
        );
        if mergeable {
            f.vqs[0].add_chain(&f.mem, &[(second, frame.len() as u32, true)]);
        }
        let mut body = Header::default().encode().to_vec();
        body.extend_from_slice(&frame);
        peers[0].send(&body).unwrap();
        wait_for("TAP RX guest buffers", || {
            f.vqs[0].used_idx(&f.mem) == if mergeable { 2 } else { 1 }
        });
        let buffers = f.mem.read_obj::<u16>(GuestAddress(first + 10)).unwrap();
        assert_eq!(buffers, if mergeable { 2 } else { 1 });
        let split = if mergeable { 13 } else { frame.len() };
        let mut received = vec![0; frame.len()];
        f.mem
            .read_slice(&mut received[..split], GuestAddress(first + 12))
            .unwrap();
        if mergeable {
            f.mem
                .read_slice(&mut received[split..], GuestAddress(second))
                .unwrap();
        }
        assert_eq!(received, frame);
        wait_for("TAP RX queue interrupt", || {
            f.irq.queues.lock().contains(&0)
        });
        assert!(lock(&f.dev.shared).tap_error.is_none());
    }
}

#[test]
fn tap_worker_mq_drains_inactive_tap_and_resteers_after_control() {
    let (mut f, peers) = tap_worker_fixture(2, false, false);
    let frame = checksum_frame(false);
    let mut body = Header::default().encode().to_vec();
    body.extend_from_slice(&frame);
    let address = f.data(0);
    f.vqs[0].add_chain(&f.mem, &[(address, 256, true)]);
    peers[1].send(&body).unwrap();
    wait_for(
        "inactive TAP queue redirected to active guest queue",
        || f.vqs[0].used_idx(&f.mem) == 1,
    );
    assert_eq!(tap_worker_set_pairs(&mut f, 3), 1);
    assert_eq!(tap_worker_set_pairs(&mut f, 2), 0);
    lock(&f.dev.shared)
        .vnet
        .as_mut()
        .unwrap()
        .steering
        .insert(flow_hash(&frame), 1);
    let address = f.data(1);
    f.vqs[2].add_chain(&f.mem, &[(address, 256, true)]);
    peers[0].send(&body).unwrap();
    wait_for("TAP RX follows guest pair steering", || {
        f.vqs[2].used_idx(&f.mem) == 1
    });
    let mut received = vec![0; frame.len()];
    f.mem
        .read_slice(&mut received, GuestAddress(address + 12))
        .unwrap();
    assert_eq!(received, frame);
    assert_eq!(tap_worker_set_pairs(&mut f, 1), 0);
    assert_eq!(lock(&f.dev.shared).vnet.as_ref().unwrap().active_pairs, 1);
}

#[test]
fn tap_worker_receive_preserves_negotiated_gso_metadata() {
    let (mut f, peers) = tap_worker_fixture(2, false, false);
    assert_eq!(tap_worker_set_pairs(&mut f, 2), 0);
    let (header, frame) = gso_packet(true, 2200);
    lock(&f.dev.shared)
        .vnet
        .as_mut()
        .unwrap()
        .steering
        .insert(flow_hash(&frame), 1);
    let address = f.data(0);
    f.vqs[2].add_chain(&f.mem, &[(address, 4096, true)]);
    let mut packet = header.encode().to_vec();
    packet.extend_from_slice(&frame);
    peers[1].send(&packet).unwrap();
    wait_for("TAP GSO RX used entry", || f.vqs[2].used_idx(&f.mem) == 1);
    let mut received = vec![0; packet.len()];
    f.mem
        .read_slice(&mut received, GuestAddress(address))
        .unwrap();
    assert_eq!(&received[..10], &packet[..10]);
    assert_eq!(&received[12..], &frame);
    assert_eq!(f.vqs[2].used_entry(&f.mem, 0).1, packet.len() as u32);
    assert!(lock(&f.dev.shared).tap_error.is_none());
}

#[test]
fn tap_worker_blocked_rx_queue_does_not_stop_other_taps_and_refill_wakes() {
    let (mut f, peers) = tap_worker_fixture(2, false, false);
    assert_eq!(tap_worker_set_pairs(&mut f, 2), 0);
    let first = checksum_frame(false);
    let mut second = first.clone();
    second[35] ^= 1;
    {
        let mut shared = lock(&f.dev.shared);
        let steering = &mut shared.vnet.as_mut().unwrap().steering;
        steering.insert(flow_hash(&first), 0);
        steering.insert(flow_hash(&second), 1);
    }
    let address = f.data(1);
    f.vqs[2].add_chain(&f.mem, &[(address, 256, true)]);
    for (peer, frame) in peers.iter().zip([&first, &second]) {
        let mut packet = Header::default().encode().to_vec();
        packet.extend_from_slice(frame);
        peer.send(&packet).unwrap();
    }
    wait_for("unblocked TAP RX queue proceeds", || {
        f.vqs[2].used_idx(&f.mem) == 1
    });
    assert_eq!(f.vqs[0].used_idx(&f.mem), 0);
    assert_eq!(lock(&f.dev.shared).rx_pending.len(), 1);
    let address = f.data(0);
    f.vqs[0].add_chain(&f.mem, &[(address, 256, true)]);
    f.dev.queue_notify(0);
    wait_for("TAP RX refill wake retries queued packet", || {
        f.vqs[0].used_idx(&f.mem) == 1
    });
    for (slot, frame) in [(0, first), (1, second)] {
        let mut received = vec![0; frame.len()];
        f.mem
            .read_slice(&mut received, GuestAddress(f.data(slot) + 12))
            .unwrap();
        assert_eq!(received, frame);
    }
    assert!(lock(&f.dev.shared).rx_pending.is_empty());
    assert!(lock(&f.dev.shared).tap_error.is_none());
}

#[test]
fn tap_worker_backpressure_keeps_tx_descriptor_until_pollout() {
    let (mut f, peers) = tap_worker_fixture(1, false, false);
    f.dev.quiesce();
    let filler = vec![0x5a; 60_000];
    let mut filled = 0;
    {
        let mut shared = lock(&f.dev.shared);
        let tap = shared.tap.as_mut().unwrap();
        loop {
            match tap.send(0, Header::default(), &filler) {
                Ok(()) => filled += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("filling packet socket: {error}"),
            }
            assert!(filled < 1000);
        }
    }
    let frame = checksum_frame(false);
    tap_worker_queue_tx(&mut f, 0, Header::default(), &frame, 0);
    f.dev.resume_snapshot();
    wait_for("TAP worker attempts backpressured TX", || {
        f.mem.read_obj::<u16>(GuestAddress(f.vqs[1].used)).unwrap() & 1 != 0
    });
    f.dev.quiesce();
    assert_eq!(f.vqs[1].used_idx(&f.mem), 0);
    assert_eq!(
        lock(&f.dev.shared).queues[1].as_ref().unwrap().next_avail(),
        0
    );
    f.dev.resume_snapshot();
    for _ in 0..filled {
        assert_eq!(&tap_worker_receive(&peers[0])[12..], &filler);
    }
    assert_eq!(&tap_worker_receive(&peers[0])[12..], &frame);
    wait_for("TAP POLLOUT retries blocked descriptor", || {
        f.vqs[1].used_idx(&f.mem) == 1
    });
    assert!(lock(&f.dev.shared).tap_error.is_none());
}

#[test]
fn tap_worker_snapshot_preserves_pending_rx_and_resumes_packet_io() {
    let (mut f, peers) = tap_worker_fixture(2, false, false);
    assert_eq!(tap_worker_set_pairs(&mut f, 2), 0);
    let frame = checksum_frame(false);
    lock(&f.dev.shared)
        .vnet
        .as_mut()
        .unwrap()
        .steering
        .insert(flow_hash(&frame), 1);
    let mut body = Header::default().encode().to_vec();
    body.extend_from_slice(&frame);
    peers[0].send(&body).unwrap();
    wait_for("TAP RX staged without guest buffers", || {
        lock(&f.dev.shared).rx_pending.len() == 1
    });
    f.dev.quiesce();
    let snapshot = f.dev.save_state().unwrap();
    {
        let mut state = lock(&f.dev.shared);
        state.rx_pending.clear();
        state.vnet.as_mut().unwrap().steering.clear();
    }
    f.dev.restore_state(&snapshot).unwrap();
    assert_eq!(lock(&f.dev.shared).rx_pending.len(), 1);
    let address = f.data(0);
    f.vqs[2].add_chain(&f.mem, &[(address, 256, true)]);
    f.dev.resume_snapshot();
    wait_for("TAP snapshot pending RX delivered", || {
        f.vqs[2].used_idx(&f.mem) == 1
    });
    let mut received = vec![0; frame.len()];
    f.mem
        .read_slice(&mut received, GuestAddress(address + 12))
        .unwrap();
    assert_eq!(received, frame);
    tap_worker_queue_tx(&mut f, 1, Header::default(), &frame, 1);
    assert_eq!(&tap_worker_receive(&peers[1])[12..], &frame);
    assert!(lock(&f.dev.shared).tap_error.is_none());
}

#[test]
fn tap_worker_kernel_activation_during_restore_waits_for_snapshot_resume() {
    let (mut f, _peers) = tap_worker_fixture(2, false, true);
    assert_eq!(tap_worker_set_pairs(&mut f, 2), 0);
    f.dev.quiesce();
    let snapshot = f.dev.save_state().unwrap();
    assert!(
        lock(&f.dev.shared)
            .tap
            .as_ref()
            .unwrap()
            .pollfds()
            .is_empty()
    );
    f.activate();
    assert!(
        lock(&f.dev.shared)
            .tap
            .as_ref()
            .unwrap()
            .pollfds()
            .is_empty()
    );
    f.dev.restore_state(&snapshot).unwrap();
    assert!(
        lock(&f.dev.shared)
            .tap
            .as_ref()
            .unwrap()
            .pollfds()
            .is_empty()
    );
    f.dev.resume_snapshot();
    assert_eq!(lock(&f.dev.shared).tap.as_ref().unwrap().pollfds().len(), 4);
    assert!(lock(&f.dev.shared).tap_error.is_none());
}

#[test]
fn tap_worker_kernel_mq_and_late_call_snapshot_keep_queue_ids() {
    let (mut f, _peers) = tap_worker_fixture(2, false, true);
    assert_eq!(tap_worker_set_pairs(&mut f, 2), 0);
    assert_eq!(lock(&f.dev.shared).tap.as_ref().unwrap().pollfds().len(), 4);
    {
        let shared = lock(&f.dev.shared);
        shared.tap.as_ref().unwrap().test_call(3);
    }
    wait_for("vhost callfd forwarded as TX queue 3 IRQ", || {
        f.irq.queues.lock().contains(&3)
    });
    f.dev.quiesce();
    f.irq.queues.lock().clear();
    {
        let shared = lock(&f.dev.shared);
        shared.tap.as_ref().unwrap().test_call(2);
    }
    f.dev.quiesce();
    assert_eq!(&*f.irq.queues.lock(), &[2]);
    let snapshot = f.dev.save_state().unwrap();
    f.dev.restore_state(&snapshot).unwrap();
    f.dev.resume_snapshot();
    assert_eq!(tap_worker_set_pairs(&mut f, 1), 0);
    assert_eq!(lock(&f.dev.shared).tap.as_ref().unwrap().pollfds().len(), 2);
    assert!(lock(&f.dev.shared).tap_error.is_none());
}
