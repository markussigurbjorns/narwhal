use media::{RtpPacket, RtpStreamInfo, is_probable_video_keyframe, stream_info};
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{info, warn};

#[derive(Clone)]
pub struct BroadcastRouter {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    subs: HashMap<String, SubscriberEntry>,
    streams: HashMap<String, RtpStreamInfo>,
}

struct SubscriberEntry {
    tx: mpsc::Sender<RtpPacket>,
    joined_at: Instant,
    first_packet_logged: bool,
    first_video_keyframe_logged: bool,
    dropped_packets: u64,
    last_drop_log: Instant,
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
        self.inner.write().subs.insert(
            sub_id,
            SubscriberEntry {
                tx,
                joined_at: Instant::now(),
                first_packet_logged: false,
                first_video_keyframe_logged: false,
                dropped_packets: 0,
                last_drop_log: Instant::now(),
            },
        );
    }

    pub fn remove_sub(&self, sub_id: &str) {
        self.inner.write().subs.remove(sub_id);
    }

    pub fn subscriber_entries(&self) -> Vec<(String, mpsc::Sender<RtpPacket>)> {
        self.inner
            .read()
            .subs
            .iter()
            .map(|(sub_id, entry)| (sub_id.clone(), entry.tx.clone()))
            .collect()
    }

    pub fn fanout(&self, pkt: RtpPacket) {
        let first_video_keyframe = is_probable_video_keyframe(&pkt);

        if let Some(info) = stream_info(&pkt) {
            self.inner
                .write()
                .streams
                .insert(info.media_key.clone(), info);
        }

        let mut stale = Vec::new();

        {
            let mut g = self.inner.write();
            for (sub_id, entry) in &mut g.subs {
                let elapsed = entry.joined_at.elapsed();
                if !entry.first_packet_logged {
                    info!(
                        subscriber = %sub_id,
                        join_to_first_rtp_ms = elapsed.as_millis(),
                        "first RTP packet forwarded to subscriber"
                    );
                    entry.first_packet_logged = true;
                }

                if first_video_keyframe && !entry.first_video_keyframe_logged {
                    info!(
                        subscriber = %sub_id,
                        join_to_first_video_keyframe_ms = elapsed.as_millis(),
                        "first video keyframe forwarded to subscriber"
                    );
                    entry.first_video_keyframe_logged = true;
                }

                match entry.tx.try_send(pkt.clone()) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // Drop overflowing RTP instead of letting a slow subscriber grow memory without bound.
                        entry.dropped_packets += 1;
                        let now = Instant::now();
                        let should_log = entry.dropped_packets <= 5
                            || entry.dropped_packets % 200 == 0
                            || now.duration_since(entry.last_drop_log) >= Duration::from_secs(2);
                        if should_log {
                            let max = entry.tx.max_capacity();
                            let depth = max.saturating_sub(entry.tx.capacity());
                            warn!(
                                subscriber = %sub_id,
                                dropped_packets = entry.dropped_packets,
                                queue_depth = depth,
                                queue_capacity = max,
                                "dropping RTP for subscriber because queue is full"
                            );
                            entry.last_drop_log = now;
                        }
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
