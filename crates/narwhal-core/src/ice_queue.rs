use std::{collections::VecDeque, sync::Arc};

use media::IceCandidate;
use parking_lot::Mutex;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct IceQueue {
    q: Arc<Mutex<VecDeque<IceCandidate>>>,
}

impl IceQueue {
    pub fn new() -> Self {
        Self {
            q: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn push(&self, c: IceCandidate) {
        self.q.lock().push_back(c);
    }

    pub fn drain(&self, max: usize) -> Vec<IceCandidate> {
        let mut q = self.q.lock();
        let mut out = Vec::new();

        for _ in 0..max {
            if let Some(c) = q.pop_front() {
                out.push(c);
            } else {
                break;
            }
        }
        out
    }
}

/// Starts a background task that collects outgoing ICE candidates from a peer
/// and pushes them into the queue.
pub fn start_ice_collector(mut rx: broadcast::Receiver<IceCandidate>, sink: IceQueue) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(c) => sink.push(c),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
