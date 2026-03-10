use std::collections::{HashMap, HashSet};

use crate::{
    BroadcastPolicyMode, MediaKind, MeetingPolicyMode, MeetingRoomState, ParticipantId, RoomId,
    TrackId,
};

#[derive(Clone, Debug)]
pub struct RoomFacts {
    pub room_id: RoomId,
    pub policy_mode: MeetingPolicyMode,
    pub revision: u64,
    pub participants: HashMap<ParticipantId, ParticipantFacts>,
    pub publications: HashMap<TrackId, PublicationFacts>,
    pub subscription_requests: HashMap<ParticipantId, HashSet<TrackId>>,
}

#[derive(Clone, Debug)]
pub struct ParticipantFacts {
    pub id: ParticipantId,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PublicationFacts {
    pub track_id: TrackId,
    pub publisher_id: ParticipantId,
    pub media_kind: MediaKind,
    pub mid: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BroadcastFacts {
    pub revision: u64,
    pub policy_mode: BroadcastPolicyMode,
    pub publisher_id: ParticipantId,
    pub subscriber_ids: Vec<ParticipantId>,
    pub audio_track_id: TrackId,
    pub video_track_id: TrackId,
}

#[derive(Clone, Debug, Default)]
pub struct SubscriptionPlan {
    pub room_revision: u64,
    pub per_subscriber: HashMap<ParticipantId, SubscriberPlan>,
    pub track_subscribers: HashMap<TrackId, Vec<ParticipantId>>,
}

impl SubscriptionPlan {
    pub fn effective_track_ids_for(&self, participant_id: &ParticipantId) -> Vec<TrackId> {
        let mut out = self
            .per_subscriber
            .get(participant_id)
            .map(|plan| {
                plan.audio
                    .iter()
                    .map(|selection| selection.track_id.clone())
                    .chain(
                        plan.video
                            .iter()
                            .map(|selection| selection.track_id.clone()),
                    )
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[derive(Clone, Debug)]
pub struct SubscriberPlan {
    pub subscriber_id: ParticipantId,
    pub video: Vec<VideoSelection>,
    pub audio: Vec<AudioSelection>,
}

#[derive(Clone, Debug)]
pub struct VideoSelection {
    pub track_id: TrackId,
    pub publisher_id: ParticipantId,
    pub mid: Option<String>,
    pub target: VideoTarget,
}

#[derive(Clone, Debug)]
pub struct AudioSelection {
    pub track_id: TrackId,
    pub publisher_id: ParticipantId,
    pub mid: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoTarget {
    High,
    Medium,
    Low,
}

pub trait PolicyEngine: Send + Sync {
    fn build_plan(&self, facts: &RoomFacts) -> SubscriptionPlan;
}

fn empty_plan(revision: u64) -> SubscriptionPlan {
    SubscriptionPlan {
        room_revision: revision,
        per_subscriber: HashMap::new(),
        track_subscribers: HashMap::new(),
    }
}

fn sort_requested_tracks(requested: HashSet<TrackId>) -> Vec<TrackId> {
    let mut ordered_tracks = requested.into_iter().collect::<Vec<_>>();
    ordered_tracks.sort_by(|a, b| a.0.cmp(&b.0));
    ordered_tracks
}

fn build_requested_track_plan<F>(facts: &RoomFacts, select_video_target: F) -> SubscriptionPlan
where
    F: Fn(usize, &PublicationFacts) -> Option<VideoTarget>,
{
    let mut plan = empty_plan(facts.revision);

    for participant in facts.participants.values() {
        let subscribed = facts
            .subscription_requests
            .get(&participant.id)
            .cloned()
            .unwrap_or_default();

        let mut subscriber_plan = SubscriberPlan {
            subscriber_id: participant.id.clone(),
            video: Vec::new(),
            audio: Vec::new(),
        };

        let ordered_tracks = sort_requested_tracks(subscribed);

        for track_id in ordered_tracks {
            let Some(publication) = facts.publications.get(&track_id) else {
                continue;
            };
            if publication.publisher_id == participant.id {
                continue;
            }

            match publication.media_kind {
                MediaKind::Audio => subscriber_plan.audio.push(AudioSelection {
                    track_id: publication.track_id.clone(),
                    publisher_id: publication.publisher_id.clone(),
                    mid: publication.mid.clone(),
                }),
                MediaKind::Video => {
                    let Some(target) = select_video_target(subscriber_plan.video.len(), publication)
                    else {
                        continue;
                    };
                    subscriber_plan.video.push(VideoSelection {
                        track_id: publication.track_id.clone(),
                        publisher_id: publication.publisher_id.clone(),
                        mid: publication.mid.clone(),
                        target,
                    });
                }
            }

            plan.track_subscribers
                .entry(publication.track_id.clone())
                .or_default()
                .push(participant.id.clone());
        }

        plan.per_subscriber
            .insert(participant.id.clone(), subscriber_plan);
    }

    plan
}

pub struct StandardMeetingPolicy;

impl PolicyEngine for StandardMeetingPolicy {
    fn build_plan(&self, facts: &RoomFacts) -> SubscriptionPlan {
        build_requested_track_plan(facts, |_selected_videos, _publication| {
            Some(VideoTarget::High)
        })
    }
}

pub struct LowBandwidthPolicy;

impl PolicyEngine for LowBandwidthPolicy {
    fn build_plan(&self, facts: &RoomFacts) -> SubscriptionPlan {
        build_requested_track_plan(facts, |selected_videos, _publication| match selected_videos {
            0 => Some(VideoTarget::High),
            1 | 2 => Some(VideoTarget::Low),
            _ => None,
        })
    }
}

pub fn build_plan_for_mode(facts: &RoomFacts) -> SubscriptionPlan {
    match facts.policy_mode {
        MeetingPolicyMode::Standard => StandardMeetingPolicy.build_plan(facts),
        MeetingPolicyMode::LowBandwidth => LowBandwidthPolicy.build_plan(facts),
    }
}

pub fn build_broadcast_plan(
    revision: u64,
    publisher_id: ParticipantId,
    subscriber_ids: impl IntoIterator<Item = ParticipantId>,
    audio_track_id: TrackId,
    video_track_id: TrackId,
) -> SubscriptionPlan {
    let mut plan = empty_plan(revision);

    for subscriber_id in subscriber_ids {
        let subscriber_plan = SubscriberPlan {
            subscriber_id: subscriber_id.clone(),
            video: vec![VideoSelection {
                track_id: video_track_id.clone(),
                publisher_id: publisher_id.clone(),
                mid: None,
                target: VideoTarget::High,
            }],
            audio: vec![AudioSelection {
                track_id: audio_track_id.clone(),
                publisher_id: publisher_id.clone(),
                mid: None,
            }],
        };

        plan.track_subscribers
            .entry(audio_track_id.clone())
            .or_default()
            .push(subscriber_id.clone());
        plan.track_subscribers
            .entry(video_track_id.clone())
            .or_default()
            .push(subscriber_id.clone());
        plan.per_subscriber.insert(subscriber_id, subscriber_plan);
    }

    plan
}

pub fn build_broadcast_plan_from_facts(facts: &BroadcastFacts) -> SubscriptionPlan {
    match facts.policy_mode {
        BroadcastPolicyMode::Standard => build_broadcast_plan(
            facts.revision,
            facts.publisher_id.clone(),
            facts.subscriber_ids.clone(),
            facts.audio_track_id.clone(),
            facts.video_track_id.clone(),
        ),
        BroadcastPolicyMode::LowBandwidth => {
            let mut plan = empty_plan(facts.revision);
            for subscriber_id in &facts.subscriber_ids {
                let subscriber_plan = SubscriberPlan {
                    subscriber_id: subscriber_id.clone(),
                    video: vec![VideoSelection {
                        track_id: facts.video_track_id.clone(),
                        publisher_id: facts.publisher_id.clone(),
                        mid: None,
                        target: VideoTarget::Low,
                    }],
                    audio: vec![AudioSelection {
                        track_id: facts.audio_track_id.clone(),
                        publisher_id: facts.publisher_id.clone(),
                        mid: None,
                    }],
                };
                plan.track_subscribers
                    .entry(facts.audio_track_id.clone())
                    .or_default()
                    .push(subscriber_id.clone());
                plan.track_subscribers
                    .entry(facts.video_track_id.clone())
                    .or_default()
                    .push(subscriber_id.clone());
                plan.per_subscriber
                    .insert(subscriber_id.clone(), subscriber_plan);
            }
            plan
        }
        BroadcastPolicyMode::AudioOnly => {
            let mut plan = empty_plan(facts.revision);
            for subscriber_id in &facts.subscriber_ids {
                let subscriber_plan = SubscriberPlan {
                    subscriber_id: subscriber_id.clone(),
                    video: Vec::new(),
                    audio: vec![AudioSelection {
                        track_id: facts.audio_track_id.clone(),
                        publisher_id: facts.publisher_id.clone(),
                        mid: None,
                    }],
                };
                plan.track_subscribers
                    .entry(facts.audio_track_id.clone())
                    .or_default()
                    .push(subscriber_id.clone());
                plan.per_subscriber
                    .insert(subscriber_id.clone(), subscriber_plan);
            }
            plan
        }
    }
}

impl RoomFacts {
    pub fn from_meeting_state(meeting: &MeetingRoomState) -> Self {
        Self {
            room_id: meeting.room_id.clone(),
            policy_mode: meeting.policy_mode,
            revision: meeting.revision,
            participants: meeting
                .participants
                .iter()
                .map(|(id, participant)| {
                    (
                        id.clone(),
                        ParticipantFacts {
                            id: participant.id.clone(),
                            display_name: participant.display_name.clone(),
                        },
                    )
                })
                .collect(),
            publications: meeting
                .publications
                .iter()
                .map(|(track_id, publication)| {
                    (
                        track_id.clone(),
                        PublicationFacts {
                            track_id: publication.track_id.clone(),
                            publisher_id: publication.publisher.clone(),
                            media_kind: publication.media_kind,
                            mid: publication.mid.clone(),
                        },
                    )
                })
                .collect(),
            subscription_requests: meeting.subscription_requests.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant_id(id: &str) -> ParticipantId {
        ParticipantId(id.to_string())
    }

    fn track_id(id: &str) -> TrackId {
        TrackId(id.to_string())
    }

    fn participant(id: &str) -> ParticipantFacts {
        ParticipantFacts {
            id: participant_id(id),
            display_name: Some(id.to_string()),
        }
    }

    fn publication(track: &str, publisher: &str, media_kind: MediaKind) -> PublicationFacts {
        PublicationFacts {
            track_id: track_id(track),
            publisher_id: participant_id(publisher),
            media_kind,
            mid: None,
        }
    }

    fn subscription_request(track_ids: &[&str]) -> HashSet<TrackId> {
        track_ids.iter().map(|id| track_id(id)).collect()
    }

    fn room_facts(policy_mode: MeetingPolicyMode) -> RoomFacts {
        let alice = participant("alice");
        let bob = participant("bob");

        let mut participants = HashMap::new();
        participants.insert(alice.id.clone(), alice);
        participants.insert(bob.id.clone(), bob);

        let mut publications = HashMap::new();
        publications.insert(
            track_id("alice-audio"),
            publication("alice-audio", "alice", MediaKind::Audio),
        );
        publications.insert(
            track_id("alice-video-1"),
            publication("alice-video-1", "alice", MediaKind::Video),
        );
        publications.insert(
            track_id("alice-video-2"),
            publication("alice-video-2", "alice", MediaKind::Video),
        );
        publications.insert(
            track_id("alice-video-3"),
            publication("alice-video-3", "alice", MediaKind::Video),
        );
        publications.insert(
            track_id("alice-video-4"),
            publication("alice-video-4", "alice", MediaKind::Video),
        );

        let mut subscription_requests = HashMap::new();
        subscription_requests.insert(
            participant_id("bob"),
            subscription_request(&[
                "alice-audio",
                "alice-video-1",
                "alice-video-2",
                "alice-video-3",
                "alice-video-4",
            ]),
        );
        subscription_requests.insert(
            participant_id("alice"),
            subscription_request(&["alice-audio", "alice-video-1"]),
        );

        RoomFacts {
            room_id: RoomId("room-1".to_string()),
            policy_mode,
            revision: 42,
            participants,
            publications,
            subscription_requests,
        }
    }

    #[test]
    fn standard_policy_keeps_all_requested_remote_tracks() {
        let facts = room_facts(MeetingPolicyMode::Standard);

        let plan = StandardMeetingPolicy.build_plan(&facts);
        let bob = participant_id("bob");

        let effective = plan.effective_track_ids_for(&bob);
        let effective_ids = effective.iter().map(|id| id.0.as_str()).collect::<Vec<_>>();

        assert_eq!(plan.room_revision, 42);
        assert_eq!(
            effective_ids,
            vec![
                "alice-audio",
                "alice-video-1",
                "alice-video-2",
                "alice-video-3",
                "alice-video-4",
            ]
        );
        let video_targets = plan
            .per_subscriber
            .get(&bob)
            .expect("bob plan")
            .video
            .iter()
            .map(|selection| selection.target)
            .collect::<Vec<_>>();
        assert_eq!(
            video_targets,
            vec![
                VideoTarget::High,
                VideoTarget::High,
                VideoTarget::High,
                VideoTarget::High,
            ]
        );
    }

    #[test]
    fn low_bandwidth_policy_caps_video_but_keeps_audio() {
        let facts = room_facts(MeetingPolicyMode::LowBandwidth);

        let plan = LowBandwidthPolicy.build_plan(&facts);
        let bob = participant_id("bob");

        let effective = plan.effective_track_ids_for(&bob);
        let effective_ids = effective.iter().map(|id| id.0.as_str()).collect::<Vec<_>>();

        assert_eq!(
            effective_ids,
            vec!["alice-audio", "alice-video-1", "alice-video-2", "alice-video-3",]
        );
        let video_targets = plan
            .per_subscriber
            .get(&bob)
            .expect("bob plan")
            .video
            .iter()
            .map(|selection| selection.target)
            .collect::<Vec<_>>();
        assert_eq!(
            video_targets,
            vec![VideoTarget::High, VideoTarget::Low, VideoTarget::Low,]
        );
    }

    #[test]
    fn requested_tracks_remain_separate_from_effective_tracks() {
        let facts = room_facts(MeetingPolicyMode::LowBandwidth);
        let requested = facts
            .subscription_requests
            .get(&participant_id("bob"))
            .expect("bob request must exist")
            .iter()
            .map(|id| id.0.as_str())
            .collect::<Vec<_>>();

        let plan = build_plan_for_mode(&facts);
        let effective_track_ids = plan.effective_track_ids_for(&participant_id("bob"));
        let effective = effective_track_ids
            .iter()
            .map(|id| id.0.as_str())
            .collect::<Vec<_>>();

        assert_eq!(requested.len(), 5);
        assert_eq!(effective.len(), 4);
        assert!(requested.contains(&"alice-video-4"));
        assert!(!effective.contains(&"alice-video-4"));
    }

    #[test]
    fn selector_uses_policy_mode() {
        let standard_plan = build_plan_for_mode(&room_facts(MeetingPolicyMode::Standard));
        let low_bandwidth_plan = build_plan_for_mode(&room_facts(MeetingPolicyMode::LowBandwidth));
        let bob = participant_id("bob");

        assert_eq!(standard_plan.effective_track_ids_for(&bob).len(), 5);
        assert_eq!(low_bandwidth_plan.effective_track_ids_for(&bob).len(), 4);
    }

    #[test]
    fn self_published_tracks_are_not_reflected_back() {
        let facts = room_facts(MeetingPolicyMode::Standard);

        let plan = StandardMeetingPolicy.build_plan(&facts);
        let alice = participant_id("alice");

        assert!(plan.effective_track_ids_for(&alice).is_empty());
    }

    #[test]
    fn broadcast_plan_includes_all_subscribers() {
        let plan = build_broadcast_plan_from_facts(&BroadcastFacts {
            revision: 9,
            policy_mode: BroadcastPolicyMode::Standard,
            publisher_id: participant_id("publisher"),
            subscriber_ids: vec![participant_id("sub-a"), participant_id("sub-b")],
            audio_track_id: track_id("__broadcast_audio"),
            video_track_id: track_id("__broadcast_video"),
        });

        assert_eq!(plan.room_revision, 9);
        assert_eq!(plan.per_subscriber.len(), 2);
        assert_eq!(
            plan.effective_track_ids_for(&participant_id("sub-a")),
            vec![track_id("__broadcast_audio"), track_id("__broadcast_video")]
        );
        assert_eq!(
            plan.track_subscribers.get(&track_id("__broadcast_video")).map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn broadcast_low_bandwidth_downgrades_video_target() {
        let plan = build_broadcast_plan_from_facts(&BroadcastFacts {
            revision: 10,
            policy_mode: BroadcastPolicyMode::LowBandwidth,
            publisher_id: participant_id("publisher"),
            subscriber_ids: vec![participant_id("sub-a")],
            audio_track_id: track_id("__broadcast_audio"),
            video_track_id: track_id("__broadcast_video"),
        });

        let subscriber = plan
            .per_subscriber
            .get(&participant_id("sub-a"))
            .expect("subscriber plan");
        assert_eq!(subscriber.video.len(), 1);
        assert_eq!(subscriber.video[0].target, VideoTarget::Low);
    }

    #[test]
    fn broadcast_audio_only_excludes_video() {
        let plan = build_broadcast_plan_from_facts(&BroadcastFacts {
            revision: 11,
            policy_mode: BroadcastPolicyMode::AudioOnly,
            publisher_id: participant_id("publisher"),
            subscriber_ids: vec![participant_id("sub-a")],
            audio_track_id: track_id("__broadcast_audio"),
            video_track_id: track_id("__broadcast_video"),
        });

        let subscriber = plan
            .per_subscriber
            .get(&participant_id("sub-a"))
            .expect("subscriber plan");
        assert!(subscriber.video.is_empty());
        assert_eq!(subscriber.audio.len(), 1);
        assert!(!plan
            .track_subscribers
            .contains_key(&track_id("__broadcast_video")));
    }
}
