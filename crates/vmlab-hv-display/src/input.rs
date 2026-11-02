use crate::{Message, invalid};
use std::collections::VecDeque;
use std::io;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const LIMIT: usize = 256;

#[derive(Default)]
struct State {
    messages: VecDeque<Message>,
    closed: bool,
    warning: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct InputQueue {
    state: Mutex<State>,
    changed: Condvar,
    space: Condvar,
}

impl InputQueue {
    pub(crate) fn push(&self, message: Message) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return Err(invalid("display server has stopped"));
            }
            if matches!(&message, Message::PointerAbs { .. })
                && let Some(last @ Message::PointerAbs { .. }) = state.messages.back_mut()
            {
                *last = message;
                return Ok(());
            }
            if state.messages.len() < LIMIT {
                state.messages.push_back(message);
                self.changed.notify_one();
                return Ok(());
            }
            let now = Instant::now();
            if state
                .warning
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5))
            {
                log::warn!("display input queue full; waiting for the guest input worker");
                state.warning = Some(now);
            }
            state = self.space.wait(state).unwrap();
        }
    }

    pub(crate) fn pop(&self) -> Option<Message> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            if let Some(message) = state.messages.pop_front() {
                self.space.notify_all();
                return Some(message);
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    pub(crate) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
        self.space.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    #[test]
    fn full_transition_queue_waits_for_space_and_shutdown_cancels_waiters() {
        for shutdown in [false, true] {
            let queue = Arc::new(InputQueue::default());
            for index in 0..LIMIT {
                queue.push(Message::Buttons { mask: index as u32 }).unwrap();
            }
            let producer = queue.clone();
            let (sender, done) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                sender
                    .send(producer.push(Message::Buttons { mask: 999 }))
                    .unwrap();
            });
            assert!(matches!(
                done.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            if shutdown {
                queue.close();
                assert!(done.recv_timeout(Duration::from_secs(3)).unwrap().is_err());
            } else {
                assert_eq!(queue.pop(), Some(Message::Buttons { mask: 0 }));
                done.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
                for mask in 1..LIMIT as u32 {
                    assert_eq!(queue.pop(), Some(Message::Buttons { mask }));
                }
                assert_eq!(queue.pop(), Some(Message::Buttons { mask: 999 }));
                queue.close();
            }
            thread.join().unwrap();
        }
    }
}
