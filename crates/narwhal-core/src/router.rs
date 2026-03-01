use media::{RtpPacket, RtpStreamInfo, stream_info};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct BroadcastRouter {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    subs: HashMap<String, mpsc::UnboundedSender<RtpPacket>>,
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

    pub fn add_sub(&self, sub_id: String, tx: mpsc::UnboundedSender<RtpPacket>) {
        self.inner.write().subs.insert(sub_id, tx);
    }

    pub fn remove_sub(&self, sub_id: &str) {
        self.inner.write().subs.remove(sub_id);
    }

    pub fn fanout(&self, pkt: RtpPacket) {
        if let Some(info) = stream_info(&pkt) {
            self.inner
                .write()
                .streams
                .insert(info.media_key.clone(), info);
        }

        let g = self.inner.read();
        for tx in g.subs.values() {
            let _ = tx.send(pkt.clone());
        }
    }

    pub fn stream_infos(&self) -> Vec<RtpStreamInfo> {
        self.inner.read().streams.values().cloned().collect()
    }
}
