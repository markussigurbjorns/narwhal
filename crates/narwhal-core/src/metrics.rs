use std::time::Duration;

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder, core::Collector,
};

#[derive(Clone)]
pub struct AppMetrics {
    registry: Registry,
    rooms_total: IntGauge,
    rooms: IntGaugeVec,
    broadcast_publishers_active: IntGauge,
    broadcast_subscribers_active: IntGauge,
    meeting_participants_active: IntGauge,
    meeting_publications_active: IntGauge,
    meeting_joins_total: IntCounter,
    meeting_leaves_total: IntCounter,
    meeting_publish_tracks_total: IntCounter,
    meeting_unpublish_tracks_total: IntCounter,
    meeting_subscribe_requests_total: IntCounter,
    meeting_unsubscribe_requests_total: IntCounter,
    meeting_sdp_offers_total: IntCounter,
    meeting_trickle_ice_total: IntCounter,
    meeting_rtp_dropped_total: IntCounter,
    broadcast_publishers_started_total: IntCounter,
    broadcast_publishers_stopped_total: IntCounter,
    broadcast_subscribers_started_total: IntCounter,
    broadcast_subscribers_stopped_total: IntCounter,
    broadcast_whip_trickle_ice_total: IntCounter,
    broadcast_whep_trickle_ice_total: IntCounter,
    broadcast_rtp_dropped_total: IntCounter,
    rtp_dropped_total: IntCounterVec,
    negotiation_total: IntCounterVec,
    peer_state_transitions_total: IntCounterVec,
    broadcast_subscribe_to_first_rtp_seconds: Histogram,
    broadcast_subscribe_to_first_video_keyframe_seconds: Histogram,
    meeting_join_to_first_rtp_seconds: Histogram,
    meeting_join_to_first_video_keyframe_seconds: Histogram,
    meeting_subscribe_to_first_rtp_seconds: Histogram,
    meeting_subscribe_to_first_video_keyframe_seconds: Histogram,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StateSnapshot {
    pub rooms_total: u64,
    pub rooms_broadcast: u64,
    pub rooms_meeting: u64,
    pub broadcast_publishers_active: u64,
    pub broadcast_subscribers_active: u64,
    pub meeting_participants_active: u64,
    pub meeting_publications_active: u64,
}

impl AppMetrics {
    fn new() -> Self {
        let registry = Registry::new();

        let rooms_total = register(
            &registry,
            IntGauge::new("narwhal_rooms_total", "Total number of known rooms.").unwrap(),
        );
        let rooms = register(
            &registry,
            IntGaugeVec::new(
                Opts::new("narwhal_rooms", "Number of rooms by active room mode."),
                &["mode"],
            )
            .unwrap(),
        );
        let broadcast_publishers_active = register(
            &registry,
            IntGauge::new(
                "narwhal_broadcast_publishers_active",
                "Active broadcast publishers.",
            )
            .unwrap(),
        );
        let broadcast_subscribers_active = register(
            &registry,
            IntGauge::new(
                "narwhal_broadcast_subscribers_active",
                "Active broadcast subscribers.",
            )
            .unwrap(),
        );
        let meeting_participants_active = register(
            &registry,
            IntGauge::new(
                "narwhal_meeting_participants_active",
                "Active meeting participants.",
            )
            .unwrap(),
        );
        let meeting_publications_active = register(
            &registry,
            IntGauge::new(
                "narwhal_meeting_publications_active",
                "Active meeting publications.",
            )
            .unwrap(),
        );
        let meeting_joins_total = register(
            &registry,
            IntCounter::new("narwhal_meeting_joins_total", "Total meeting joins.").unwrap(),
        );
        let meeting_leaves_total = register(
            &registry,
            IntCounter::new("narwhal_meeting_leaves_total", "Total meeting leaves.").unwrap(),
        );
        let meeting_publish_tracks_total = register(
            &registry,
            IntCounter::new(
                "narwhal_meeting_publish_tracks_total",
                "Total meeting tracks published.",
            )
            .unwrap(),
        );
        let meeting_unpublish_tracks_total = register(
            &registry,
            IntCounter::new(
                "narwhal_meeting_unpublish_tracks_total",
                "Total meeting tracks unpublished.",
            )
            .unwrap(),
        );
        let meeting_subscribe_requests_total = register(
            &registry,
            IntCounter::new(
                "narwhal_meeting_subscribe_requests_total",
                "Total meeting subscribe requests.",
            )
            .unwrap(),
        );
        let meeting_unsubscribe_requests_total = register(
            &registry,
            IntCounter::new(
                "narwhal_meeting_unsubscribe_requests_total",
                "Total meeting unsubscribe requests.",
            )
            .unwrap(),
        );
        let meeting_sdp_offers_total = register(
            &registry,
            IntCounter::new(
                "narwhal_meeting_sdp_offers_total",
                "Total meeting SDP offers processed.",
            )
            .unwrap(),
        );
        let meeting_trickle_ice_total = register(
            &registry,
            IntCounter::new(
                "narwhal_meeting_trickle_ice_total",
                "Total meeting ICE candidates received from clients.",
            )
            .unwrap(),
        );
        let meeting_rtp_dropped_total = register(
            &registry,
            IntCounter::new(
                "narwhal_meeting_rtp_dropped_total",
                "Total meeting RTP packets dropped because subscriber queues were full.",
            )
            .unwrap(),
        );
        let broadcast_publishers_started_total = register(
            &registry,
            IntCounter::new(
                "narwhal_broadcast_publishers_started_total",
                "Total broadcast publishers started.",
            )
            .unwrap(),
        );
        let broadcast_publishers_stopped_total = register(
            &registry,
            IntCounter::new(
                "narwhal_broadcast_publishers_stopped_total",
                "Total broadcast publishers stopped.",
            )
            .unwrap(),
        );
        let broadcast_subscribers_started_total = register(
            &registry,
            IntCounter::new(
                "narwhal_broadcast_subscribers_started_total",
                "Total broadcast subscribers started.",
            )
            .unwrap(),
        );
        let broadcast_subscribers_stopped_total = register(
            &registry,
            IntCounter::new(
                "narwhal_broadcast_subscribers_stopped_total",
                "Total broadcast subscribers stopped.",
            )
            .unwrap(),
        );
        let broadcast_whip_trickle_ice_total = register(
            &registry,
            IntCounter::new(
                "narwhal_broadcast_whip_trickle_ice_total",
                "Total broadcast publisher ICE candidates received from clients.",
            )
            .unwrap(),
        );
        let broadcast_whep_trickle_ice_total = register(
            &registry,
            IntCounter::new(
                "narwhal_broadcast_whep_trickle_ice_total",
                "Total broadcast subscriber ICE candidates received from clients.",
            )
            .unwrap(),
        );
        let broadcast_rtp_dropped_total = register(
            &registry,
            IntCounter::new(
                "narwhal_broadcast_rtp_dropped_total",
                "Total broadcast RTP packets dropped because subscriber queues were full.",
            )
            .unwrap(),
        );
        let rtp_dropped_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "narwhal_rtp_dropped_total",
                    "RTP packets not forwarded by mode and reason.",
                ),
                &["mode", "reason"],
            )
            .unwrap(),
        );
        let negotiation_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "narwhal_negotiation_total",
                    "Negotiation attempts by flow, outcome, and cause.",
                ),
                &["flow", "outcome", "cause"],
            )
            .unwrap(),
        );
        let peer_state_transitions_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "narwhal_peer_state_transitions_total",
                    "Peer state transitions by role and state.",
                ),
                &["role", "state"],
            )
            .unwrap(),
        );
        let broadcast_subscribe_to_first_rtp_seconds = register(
            &registry,
            histogram(
                "narwhal_broadcast_subscribe_to_first_rtp_seconds",
                "Time from broadcast subscriber creation to first forwarded RTP packet.",
            ),
        );
        let broadcast_subscribe_to_first_video_keyframe_seconds = register(
            &registry,
            histogram(
                "narwhal_broadcast_subscribe_to_first_video_keyframe_seconds",
                "Time from broadcast subscriber creation to first forwarded video keyframe.",
            ),
        );
        let meeting_join_to_first_rtp_seconds = register(
            &registry,
            histogram(
                "narwhal_meeting_join_to_first_rtp_seconds",
                "Time from meeting join to first forwarded RTP packet.",
            ),
        );
        let meeting_join_to_first_video_keyframe_seconds = register(
            &registry,
            histogram(
                "narwhal_meeting_join_to_first_video_keyframe_seconds",
                "Time from meeting join to first forwarded video keyframe.",
            ),
        );
        let meeting_subscribe_to_first_rtp_seconds = register(
            &registry,
            histogram(
                "narwhal_meeting_subscribe_to_first_rtp_seconds",
                "Time from meeting subscribe request to first forwarded RTP packet.",
            ),
        );
        let meeting_subscribe_to_first_video_keyframe_seconds = register(
            &registry,
            histogram(
                "narwhal_meeting_subscribe_to_first_video_keyframe_seconds",
                "Time from meeting subscribe request to first forwarded video keyframe.",
            ),
        );

        Self {
            registry,
            rooms_total,
            rooms,
            broadcast_publishers_active,
            broadcast_subscribers_active,
            meeting_participants_active,
            meeting_publications_active,
            meeting_joins_total,
            meeting_leaves_total,
            meeting_publish_tracks_total,
            meeting_unpublish_tracks_total,
            meeting_subscribe_requests_total,
            meeting_unsubscribe_requests_total,
            meeting_sdp_offers_total,
            meeting_trickle_ice_total,
            meeting_rtp_dropped_total,
            broadcast_publishers_started_total,
            broadcast_publishers_stopped_total,
            broadcast_subscribers_started_total,
            broadcast_subscribers_stopped_total,
            broadcast_whip_trickle_ice_total,
            broadcast_whep_trickle_ice_total,
            broadcast_rtp_dropped_total,
            rtp_dropped_total,
            negotiation_total,
            peer_state_transitions_total,
            broadcast_subscribe_to_first_rtp_seconds,
            broadcast_subscribe_to_first_video_keyframe_seconds,
            meeting_join_to_first_rtp_seconds,
            meeting_join_to_first_video_keyframe_seconds,
            meeting_subscribe_to_first_rtp_seconds,
            meeting_subscribe_to_first_video_keyframe_seconds,
        }
    }

    pub fn inc_meeting_joins(&self) {
        self.meeting_joins_total.inc();
    }

    pub fn inc_meeting_leaves(&self) {
        self.meeting_leaves_total.inc();
    }

    pub fn add_meeting_publish_tracks(&self, count: u64) {
        self.meeting_publish_tracks_total.inc_by(count);
    }

    pub fn add_meeting_unpublish_tracks(&self, count: u64) {
        self.meeting_unpublish_tracks_total.inc_by(count);
    }

    pub fn inc_meeting_subscribe_requests(&self) {
        self.meeting_subscribe_requests_total.inc();
    }

    pub fn inc_meeting_unsubscribe_requests(&self) {
        self.meeting_unsubscribe_requests_total.inc();
    }

    pub fn inc_meeting_sdp_offers(&self) {
        self.meeting_sdp_offers_total.inc();
    }

    pub fn inc_meeting_trickle_ice(&self) {
        self.meeting_trickle_ice_total.inc();
    }

    pub fn inc_meeting_rtp_dropped(&self, reason: &str) {
        self.meeting_rtp_dropped_total.inc();
        self.rtp_dropped_total
            .with_label_values(&["meeting", reason])
            .inc();
    }

    pub fn inc_broadcast_publishers_started(&self) {
        self.broadcast_publishers_started_total.inc();
    }

    pub fn inc_broadcast_publishers_stopped(&self) {
        self.broadcast_publishers_stopped_total.inc();
    }

    pub fn inc_broadcast_subscribers_started(&self) {
        self.broadcast_subscribers_started_total.inc();
    }

    pub fn inc_broadcast_subscribers_stopped(&self) {
        self.broadcast_subscribers_stopped_total.inc();
    }

    pub fn inc_broadcast_whip_trickle_ice(&self) {
        self.broadcast_whip_trickle_ice_total.inc();
    }

    pub fn inc_broadcast_whep_trickle_ice(&self) {
        self.broadcast_whep_trickle_ice_total.inc();
    }

    pub fn inc_broadcast_rtp_dropped(&self, reason: &str) {
        self.broadcast_rtp_dropped_total.inc();
        self.rtp_dropped_total
            .with_label_values(&["broadcast", reason])
            .inc();
    }

    pub fn observe_negotiation(&self, flow: &str, success: bool, cause: &str) {
        let outcome = if success { "success" } else { "failure" };
        self.negotiation_total
            .with_label_values(&[flow, outcome, cause])
            .inc();
    }

    pub fn observe_peer_state(&self, role: &str, state: &str) {
        self.peer_state_transitions_total
            .with_label_values(&[role, state])
            .inc();
    }

    pub fn observe_broadcast_subscribe_to_first_rtp(&self, duration: Duration) {
        self.broadcast_subscribe_to_first_rtp_seconds
            .observe(duration.as_secs_f64());
    }

    pub fn observe_broadcast_subscribe_to_first_video_keyframe(&self, duration: Duration) {
        self.broadcast_subscribe_to_first_video_keyframe_seconds
            .observe(duration.as_secs_f64());
    }

    pub fn observe_meeting_join_to_first_rtp(&self, duration: Duration) {
        self.meeting_join_to_first_rtp_seconds
            .observe(duration.as_secs_f64());
    }

    pub fn observe_meeting_join_to_first_video_keyframe(&self, duration: Duration) {
        self.meeting_join_to_first_video_keyframe_seconds
            .observe(duration.as_secs_f64());
    }

    pub fn observe_meeting_subscribe_to_first_rtp(&self, duration: Duration) {
        self.meeting_subscribe_to_first_rtp_seconds
            .observe(duration.as_secs_f64());
    }

    pub fn observe_meeting_subscribe_to_first_video_keyframe(&self, duration: Duration) {
        self.meeting_subscribe_to_first_video_keyframe_seconds
            .observe(duration.as_secs_f64());
    }

    pub fn render_prometheus(&self, snapshot: StateSnapshot) -> String {
        self.rooms_total.set(snapshot.rooms_total as i64);
        self.rooms
            .with_label_values(&["broadcast"])
            .set(snapshot.rooms_broadcast as i64);
        self.rooms
            .with_label_values(&["meeting"])
            .set(snapshot.rooms_meeting as i64);
        self.broadcast_publishers_active
            .set(snapshot.broadcast_publishers_active as i64);
        self.broadcast_subscribers_active
            .set(snapshot.broadcast_subscribers_active as i64);
        self.meeting_participants_active
            .set(snapshot.meeting_participants_active as i64);
        self.meeting_publications_active
            .set(snapshot.meeting_publications_active as i64);

        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        TextEncoder::new()
            .encode(&metric_families, &mut buffer)
            .expect("prometheus text encoding should succeed");
        String::from_utf8(buffer).expect("prometheus text output should be valid utf-8")
    }
}

impl Default for AppMetrics {
    fn default() -> Self {
        Self::new()
    }
}

fn register<T>(registry: &Registry, metric: T) -> T
where
    T: Collector + Clone + 'static,
{
    registry
        .register(Box::new(metric.clone()))
        .expect("metric registration should succeed");
    metric
}

fn histogram(name: &str, help: &str) -> Histogram {
    Histogram::with_opts(
        HistogramOpts::new(name, help)
            .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0]),
    )
    .expect("histogram creation should succeed")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{AppMetrics, StateSnapshot};

    #[test]
    fn renders_labeled_metrics() {
        let metrics = AppMetrics::default();
        metrics.observe_negotiation("meeting_sdp_offer", false, "room_not_found");
        metrics.inc_meeting_rtp_dropped("queue_full");
        metrics.observe_meeting_join_to_first_rtp(Duration::from_millis(150));

        let rendered = metrics.render_prometheus(StateSnapshot {
            rooms_total: 2,
            rooms_broadcast: 1,
            rooms_meeting: 1,
            broadcast_publishers_active: 1,
            broadcast_subscribers_active: 3,
            meeting_participants_active: 4,
            meeting_publications_active: 5,
        });

        assert!(rendered.contains("narwhal_rooms_total 2"));
        assert!(rendered.contains("narwhal_rooms{mode=\"broadcast\"} 1"));
        assert!(rendered.contains("narwhal_rooms{mode=\"meeting\"} 1"));
        assert!(rendered.contains(
            "narwhal_negotiation_total{cause=\"room_not_found\",flow=\"meeting_sdp_offer\",outcome=\"failure\"} 1"
        ));
        assert!(
            rendered
                .contains("narwhal_rtp_dropped_total{mode=\"meeting\",reason=\"queue_full\"} 1")
        );
        assert!(rendered.contains("narwhal_meeting_join_to_first_rtp_seconds_bucket"));
    }
}
