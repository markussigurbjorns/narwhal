use media::{RtpPacket, RtpStreamInfo, stream_info};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::{self, error::TrySendError};

#[derive(Clone)]
pub struct BroadcastRouter {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    subs: HashMap<String, mpsc::Sender<RtpPacket>>,
    streams: HashMap<String, RtpStreamInfo>,
}

impl BroadcastRouter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                subs: HashMap::new(),
                streams: HashMap::new(),
            })),
        }
    }

    pub fn add_sub(&self, sub_id: String, tx: mpsc::Sender<RtpPacket>) {
        self.inner.write().subs.insert(sub_id, tx);
    }

    pub fn remove_sub(&self, sub_id: &str) {
        self.inner.write().subs.remove(sub_id);
    }

    pub fn subscriber_entries(&self) -> Vec<(String, mpsc::Sender<RtpPacket>)> {
        self.inner
            .read()
            .subs
            .iter()
            .map(|(sub_id, tx)| (sub_id.clone(), tx.clone()))
            .collect()
    }

    pub fn fanout(&self, pkt: RtpPacket) {
        if let Some(info) = stream_info(&pkt) {
            self.inner
                .write()
                .streams
                .insert(info.media_key.clone(), info);
        }

        let mut stale = Vec::new();

        {
            let g = self.inner.read();
            for (sub_id, tx) in &g.subs {
                match tx.try_send(pkt.clone()) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // Drop overflowing RTP instead of letting a slow subscriber grow memory without bound.
                    }
                    Err(TrySendError::Closed(_)) => stale.push(sub_id.clone()),
                }
            }
        }

        if !stale.is_empty() {
            let mut g = self.inner.write();
            for sub_id in stale {
                g.subs.remove(&sub_id);
            }
        }
    }

    pub fn stream_infos(&self) -> Vec<RtpStreamInfo> {
        self.inner.read().streams.values().cloned().collect()
    }
}
