//! Userspace TAP packet I/O and vhost call-event forwarding.

use std::io;
use std::sync::Arc;

use virtio_queue::{QueueOwnedT, QueueT};
use vm_memory::GuestMemoryBackend;

use super::{
    F_GUEST_CSUM, F_MRG_RXBUF, Header, MAX_FRAME, METADATA_LEN, Shared, TxPacket, hdr_len, lock,
    needs_interrupt, place_rx_frames, prepare_vnet_tx, process_control,
};
use parking_lot::Mutex;

pub(super) fn process_tap(shared: &Arc<Mutex<Shared>>) {
    process_control(shared);
    let mut notifications = Vec::new();
    let irq;
    {
        let mut guard = lock(shared);
        let sh = &mut *guard;
        if sh.snapshot_paused {
            return;
        }
        let Some(tap) = sh.tap.as_mut() else { return };
        if tap.kernel() {
            for (index, _) in tap.pollfds() {
                match tap.consume_call(index) {
                    Ok(()) => notifications.push(index),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => {
                        sh.tap_error = Some(error.to_string());
                    }
                }
            }
        } else {
            let pairs = sh.vnet.as_ref().map_or(1, |state| state.max_pairs);
            let active = sh.vnet.as_ref().map_or(1, |state| state.active_pairs);
            for pair in 0..pairs {
                for _ in 0..64 {
                    match tap.receive(pair) {
                        Ok(Some(packet)) => {
                            if let Some(state) = sh.vnet.as_ref() {
                                let hash = super::flow_hash(&packet[METADATA_LEN..]);
                                let destination = state
                                    .steering
                                    .get(&hash)
                                    .copied()
                                    .unwrap_or((hash % u32::from(active)) as u16)
                                    % active;
                                let bytes =
                                    sh.rx_pending
                                        .iter()
                                        .filter(|body| {
                                            Header::from_metadata(body).is_ok_and(
                                                |(_, _, frame)| {
                                                    let hash = super::flow_hash(frame);
                                                    state.steering.get(&hash).copied().unwrap_or(
                                                        (hash % u32::from(active)) as u16,
                                                    ) % active
                                                        == destination
                                                },
                                            )
                                        })
                                        .map(Vec::len)
                                        .sum::<usize>();
                                if bytes.saturating_add(packet.len()) <= super::MAX_TX_STAGE {
                                    sh.rx_pending.push_back(packet);
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            sh.tap_error = Some(error.to_string());
                            break;
                        }
                    }
                }
            }
            if let Some(mem) = sh.mem.as_ref() {
                for pair in 0..active {
                    let index = pair * 2 + 1;
                    let Some(queue) = sh
                        .queues
                        .get_mut(usize::from(index))
                        .and_then(Option::as_mut)
                    else {
                        continue;
                    };
                    if !queue.ready() {
                        continue;
                    }
                    let features = sh.vnet.as_ref().map_or(0, |state| state.features);
                    let hdr = hdr_len(features);
                    let mut used = false;
                    let mut blocked = false;
                    let mut invalid = false;
                    let _ = queue.disable_notification(mem);
                    for _ in 0..usize::from(queue.size()) {
                        let chain = match queue.iter(mem) {
                            Ok(mut available) => available.next(),
                            Err(_) => {
                                invalid = true;
                                break;
                            }
                        };
                        let Some(chain) = chain else { break };
                        let head = chain.head_index();
                        let mut packet = TxPacket {
                            head,
                            consumed: 0,
                            prefix: None,
                            slices: Vec::new(),
                            owned: None,
                        };
                        let mut header = [0; 12];
                        let mut skip = hdr;
                        let mut total = 0usize;
                        let mut valid = true;
                        for descriptor in chain.readable() {
                            let Ok(slice) =
                                mem.get_slice(descriptor.addr(), descriptor.len() as usize)
                            else {
                                valid = false;
                                break;
                            };
                            let Some(length) = total.checked_add(slice.len()) else {
                                valid = false;
                                break;
                            };
                            total = length;
                            if total > hdr + MAX_FRAME {
                                valid = false;
                                break;
                            }
                            let take = skip.min(slice.len());
                            if take != 0 {
                                slice
                                    .subslice(0, take)
                                    .unwrap()
                                    .copy_to(&mut header[hdr - skip..hdr - skip + take]);
                                skip -= take;
                            }
                            if take != slice.len() {
                                packet.slices.push(slice.offset(take).unwrap());
                            }
                        }
                        let len = total.saturating_sub(hdr);
                        if valid && skip == 0 {
                            if let Some(state) = sh.vnet.as_mut() {
                                valid = prepare_vnet_tx(&mut packet, &header, len, pair, state);
                            } else {
                                valid = false;
                            }
                        } else {
                            valid = false;
                        }
                        if valid {
                            let body = packet.owned.as_ref().unwrap();
                            let metadata = Header::decode(&header).unwrap();
                            match tap.send(pair, metadata, &body[METADATA_LEN..]) {
                                Ok(()) => {}
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                    ) =>
                                {
                                    queue.go_to_previous_position();
                                    let _ = tap.tx_pending(pair, true);
                                    blocked = true;
                                    break;
                                }
                                Err(error) => {
                                    sh.tap_error = Some(error.to_string());
                                }
                            }
                        }
                        if queue
                            .add_used(mem, head, if valid { total as u32 } else { 0 })
                            .is_err()
                        {
                            invalid = true;
                            break;
                        }
                        used = true;
                    }
                    if !blocked && !invalid {
                        let raced = queue.enable_notification(mem).unwrap_or(false);
                        let _ = tap.tx_pending(pair, raced);
                    }
                    if used && needs_interrupt(queue, mem) {
                        notifications.push(index);
                    }
                }
            }
        }
        irq = sh.interrupt.clone();
    }
    if let Some(irq) = irq {
        for index in notifications {
            irq.notify_queue(index);
        }
    }
    let features = lock(shared).vnet.as_ref().map_or(0, |state| state.features);
    place_rx_frames(
        shared,
        hdr_len(features),
        features & F_MRG_RXBUF != 0,
        features & F_GUEST_CSUM != 0,
    );
}
