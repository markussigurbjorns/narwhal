use std::collections::HashMap;

use crate::{ParticipantId, SubscriptionPlan, TrackId, VideoTarget};

#[derive(Clone, Debug, Default)]
pub struct ForwardingGraph {
    pub track_subscribers: HashMap<TrackId, Vec<SubscriberForwarding>>,
}

#[derive(Clone, Debug)]
pub struct SubscriberForwarding {
    pub subscriber_id: ParticipantId,
    pub kind: ForwardingKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardingKind {
    Audio,
    Video(VideoForwarding),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoForwarding {
    pub target: VideoTarget,
    pub spatial_layer: Option<u8>,
    pub temporal_layer: Option<u8>,
    pub preferred_encoding_id: Option<String>,
}

fn video_forwarding_for_target(target: VideoTarget) -> VideoForwarding {
    let (spatial_layer, temporal_layer) = match target {
        VideoTarget::High => (Some(2), Some(2)),
        VideoTarget::Medium => (Some(1), Some(1)),
        VideoTarget::Low => (Some(0), Some(0)),
    };

    VideoForwarding {
        target,
        spatial_layer,
        temporal_layer,
        preferred_encoding_id: None,
    }
}

impl ForwardingGraph {
    pub fn for_track(&self, track_id: &TrackId) -> &[SubscriberForwarding] {
        self.track_subscribers
            .get(track_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

pub fn compile_forwarding_graph(plan: &SubscriptionPlan) -> ForwardingGraph {
    let mut graph = ForwardingGraph::default();

    for subscriber_plan in plan.per_subscriber.values() {
        for selection in &subscriber_plan.audio {
            graph
                .track_subscribers
                .entry(selection.track_id.clone())
                .or_default()
                .push(SubscriberForwarding {
                    subscriber_id: subscriber_plan.subscriber_id.clone(),
                    kind: ForwardingKind::Audio,
                });
        }

        for selection in &subscriber_plan.video {
            graph
                .track_subscribers
                .entry(selection.track_id.clone())
                .or_default()
                .push(SubscriberForwarding {
                    subscriber_id: subscriber_plan.subscriber_id.clone(),
                    kind: ForwardingKind::Video(video_forwarding_for_target(selection.target)),
                });
        }
    }

    graph
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{
        AudioSelection, ParticipantId, SubscriberPlan, SubscriptionPlan, TrackId, VideoSelection,
        VideoTarget,
    };

    use super::{ForwardingGraph, ForwardingKind, VideoForwarding, compile_forwarding_graph};

    fn participant_id(id: &str) -> ParticipantId {
        ParticipantId(id.to_string())
    }

    fn track_id(id: &str) -> TrackId {
        TrackId(id.to_string())
    }

    #[test]
    fn compile_forwarding_graph_preserves_audio_and_video_targets() {
        let bob = participant_id("bob");
        let plan = SubscriptionPlan {
            room_revision: 7,
            per_subscriber: HashMap::from([(
                bob.clone(),
                SubscriberPlan {
                    subscriber_id: bob.clone(),
                    audio: vec![AudioSelection {
                        track_id: track_id("alice-audio"),
                        publisher_id: participant_id("alice"),
                        mid: None,
                    }],
                    video: vec![VideoSelection {
                        track_id: track_id("alice-video"),
                        publisher_id: participant_id("alice"),
                        mid: None,
                        target: VideoTarget::Low,
                    }],
                },
            )]),
            track_subscribers: HashMap::new(),
        };

        let graph = compile_forwarding_graph(&plan);
        let audio = graph.for_track(&track_id("alice-audio"));
        let video = graph.for_track(&track_id("alice-video"));

        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].subscriber_id, bob);
        assert_eq!(audio[0].kind, ForwardingKind::Audio);

        assert_eq!(video.len(), 1);
        assert_eq!(video[0].subscriber_id, participant_id("bob"));
        assert_eq!(
            video[0].kind,
            ForwardingKind::Video(VideoForwarding {
                target: VideoTarget::Low,
                spatial_layer: Some(0),
                temporal_layer: Some(0),
                preferred_encoding_id: None,
            })
        );
    }

    #[test]
    fn missing_track_returns_empty_slice() {
        let graph = ForwardingGraph::default();
        assert!(graph.for_track(&track_id("missing")).is_empty());
    }
}
