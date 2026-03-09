use crate::BroadcastRouter;
use crate::{
    ForwardingGraph, ForwardingKind, IceQueue, MeetingParticipantSession, MeetingPolicyMode,
    MeetingRoomState, MidRoutingInfo, ParticipantId, ParticipantState, PublisherSession,
    RoomFacts, RoomMode, SubscriberSession, SubscriptionPlan, VideoForwarding,
    compile_forwarding_graph,
    start_ice_collector,
};
use anyhow::Result;
use media::{GstRuntime, PeerEvents, PeerRole, PeerSession, PeerState, RtpPacket};
use parking_lot::RwLock;
use std::{collections::{HashMap, HashSet}, sync::Arc};
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoomId(pub String);

pub struct SdpResponse {
    pub answer_sdp: String,
    pub location: String,
}

pub struct MeetingSdpResponse {
    pub answer_sdp: String,
    pub revision: u64,
}

pub struct MeetingPublishTrack {
    pub track_id: String,
    pub media_kind: crate::MediaKind,
    pub mid: Option<String>,
}

pub struct MeetingPublicationInfo {
    pub track_id: String,
    pub publisher_id: String,
    pub media_kind: crate::MediaKind,
    pub mid: Option<String>,
}

pub struct MeetingParticipantInfo {
    pub participant_id: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MeetingStreamInfo {
    pub track_id: String,
    pub publisher_id: String,
    pub media_kind: crate::MediaKind,
    pub ssrc: u32,
    pub encoding_id: String,
    pub mid: Option<String>,
    pub rid: Option<String>,
    pub spatial_layer: Option<u8>,
}

#[derive(Clone)]
pub struct RoomManager {
    gst: GstRuntime,
    inner: Arc<RwLock<Rooms>>,
}

struct Rooms {
    rooms: HashMap<RoomId, RoomState>,
}

struct RoomState {
    mode: RoomMode,
    publisher: Option<PublisherSession>,
    subscribers: HashMap<String, SubscriberSession>,
    meeting_participants: HashMap<String, MeetingParticipantSession>,
    router: BroadcastRouter,
    meeting: MeetingRoomState,
    meeting_plan: SubscriptionPlan,
    meeting_graph: ForwardingGraph,
    meeting_streams: HashMap<crate::TrackId, HashMap<u32, MeetingStreamInfo>>,
    meeting_route_log_once: HashSet<String>,
}

struct RoomPeerEvents {
    rooms: RoomManager,
    room: RoomId,
    tokio_handle: Handle,
}

impl PeerEvents for RoomPeerEvents {
    fn on_state(&self, _peer_id: &String, _state: PeerState) {}

    fn on_keyframe_request(&self, _peer_id: &String) {
        let rooms = self.rooms.clone();
        let room = self.room.clone();
        self.tokio_handle.spawn(async move {
            if let Err(err) = rooms.request_publisher_keyframe(room).await {
                tracing::warn!("failed to service subscriber keyframe request: {err:#}");
            }
        });
    }
}

impl RoomManager {
    pub fn new(gst: GstRuntime) -> Self {
        Self {
            gst,
            inner: Arc::new(RwLock::new(Rooms {
                rooms: HashMap::new(),
            })),
        }
    }

    pub fn room_mode(&self, room: &RoomId) -> Option<RoomMode> {
        let g = self.inner.read();
        g.rooms.get(room).map(|rs| rs.mode)
    }

    pub fn ensure_room_mode(&self, room: RoomId, mode: RoomMode) {
        let mut g = self.inner.write();
        let rs = Self::room_mut(&mut g, &room);
        rs.mode = mode;
    }

    fn room_mut<'a>(rooms: &'a mut Rooms, room: &RoomId) -> &'a mut RoomState {
        rooms
            .rooms
            .entry(room.clone())
            .or_insert_with(|| RoomState {
                mode: RoomMode::Broadcast,
                publisher: None,
                subscribers: HashMap::new(),
                meeting_participants: HashMap::new(),
                router: BroadcastRouter::new(),
                meeting: MeetingRoomState::new(room.clone()),
                meeting_plan: SubscriptionPlan::default(),
                meeting_graph: ForwardingGraph::default(),
                meeting_streams: HashMap::new(),
                meeting_route_log_once: HashSet::new(),
            })
    }

    fn recompute_meeting_plan(rs: &mut RoomState) {
        rs.meeting_plan = crate::build_plan_for_mode(&RoomFacts::from_meeting_state(&rs.meeting));
        rs.meeting_graph = compile_forwarding_graph(&rs.meeting_plan);
        hydrate_meeting_graph(&mut rs.meeting_graph, &rs.meeting_streams);
    }

    pub fn meeting_policy_mode(&self, room: RoomId) -> Result<MeetingPolicyMode> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;
        Ok(rs.meeting.policy_mode)
    }

    pub fn meeting_set_policy_mode(
        &self,
        room: RoomId,
        policy_mode: MeetingPolicyMode,
    ) -> Result<u64> {
        let mut g = self.inner.write();
        let rs = g
            .rooms
            .get_mut(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;
        rs.meeting.policy_mode = policy_mode;
        let revision = rs.meeting.next_revision();
        Self::recompute_meeting_plan(rs);
        Ok(revision)
    }

    fn require_broadcast_mode(rs: &RoomState) -> Result<()> {
        match rs.mode {
            RoomMode::Broadcast => {
                // Keep meeting scaffolding initialized and validated even in broadcast rooms.
                let _meeting_revision = rs.meeting.revision;
                Ok(())
            }
            RoomMode::Meeting => Err(anyhow::anyhow!(
                "room is in meeting mode; WHIP/WHEP broadcast endpoints are not available"
            )),
        }
    }

    fn require_meeting_mode(rs: &RoomState) -> Result<()> {
        match rs.mode {
            RoomMode::Meeting => Ok(()),
            RoomMode::Broadcast => Err(anyhow::anyhow!(
                "room is in broadcast mode; meeting websocket signaling is not available"
            )),
        }
    }

    pub fn meeting_join(
        &self,
        room: RoomId,
        participant_id: String,
        display_name: Option<String>,
    ) -> Result<u64> {
        let mut g = self.inner.write();
        let rs = Self::room_mut(&mut g, &room);
        if matches!(rs.mode, RoomMode::Broadcast)
            && (!rs.subscribers.is_empty() || rs.publisher.is_some())
        {
            return Err(anyhow::anyhow!(
                "room already active in broadcast mode and cannot switch to meeting mode"
            ));
        }
        rs.mode = RoomMode::Meeting;
        Self::require_meeting_mode(rs)?;

        let pid = ParticipantId(participant_id);
        rs.meeting
            .participants
            .entry(pid.clone())
            .or_insert(ParticipantState {
                id: pid,
                display_name,
            });
        let rev = rs.meeting.next_revision();
        Self::recompute_meeting_plan(rs);
        Ok(rev)
    }

    pub fn meeting_publish_tracks(
        &self,
        room: RoomId,
        participant_id: &str,
        tracks: Vec<MeetingPublishTrack>,
    ) -> Result<u64> {
        let mut g = self.inner.write();
        let rs = g
            .rooms
            .get_mut(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let pid = ParticipantId(participant_id.to_string());
        if !rs.meeting.participants.contains_key(&pid) {
            return Err(anyhow::anyhow!("participant not joined"));
        }

        for track in tracks {
            let tid = crate::TrackId(track.track_id);
            rs.meeting.publications.insert(
                tid.clone(),
                crate::Publication {
                    track_id: tid,
                    publisher: pid.clone(),
                    media_kind: track.media_kind,
                    mid: track.mid,
                },
            );
        }

        let revision = rs.meeting.next_revision();
        Self::recompute_meeting_plan(rs);
        Ok(revision)
    }

    pub fn meeting_unpublish_tracks(
        &self,
        room: RoomId,
        participant_id: &str,
        track_ids: Vec<String>,
    ) -> Result<u64> {
        let mut g = self.inner.write();
        let rs = g
            .rooms
            .get_mut(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let pid = ParticipantId(participant_id.to_string());
        if !rs.meeting.participants.contains_key(&pid) {
            return Err(anyhow::anyhow!("participant not joined"));
        }

        for raw_id in track_ids {
            let tid = crate::TrackId(raw_id);
            if let Some(publi) = rs.meeting.publications.get(&tid) {
                if publi.publisher != pid {
                    return Err(anyhow::anyhow!(
                        "cannot unpublish track not owned by participant"
                    ));
                }
            }
            rs.meeting.publications.remove(&tid);
            rs.meeting_streams.remove(&tid);
            for subs in rs.meeting.subscription_requests.values_mut() {
                subs.remove(&tid);
            }
        }

        let revision = rs.meeting.next_revision();
        Self::recompute_meeting_plan(rs);
        Ok(revision)
    }

    pub fn meeting_subscribe(
        &self,
        room: RoomId,
        participant_id: &str,
        track_ids: Vec<String>,
    ) -> Result<u64> {
        let mut g = self.inner.write();
        let rs = g
            .rooms
            .get_mut(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let pid = ParticipantId(participant_id.to_string());
        if !rs.meeting.participants.contains_key(&pid) {
            return Err(anyhow::anyhow!("participant not joined"));
        }

        let wanted: Result<Vec<crate::TrackId>> = track_ids
            .into_iter()
            .map(|id| {
                let tid = crate::TrackId(id);
                if !rs.meeting.publications.contains_key(&tid) {
                    return Err(anyhow::anyhow!("unknown track requested"));
                }
                Ok(tid)
            })
            .collect();

        let wanted = wanted?;
        let mut new_video_publishers = HashSet::new();
        let subs = rs.meeting.subscription_requests.entry(pid).or_default();
        for tid in wanted {
            let is_new = subs.insert(tid.clone());
            if is_new
                && let Some(publication) = rs.meeting.publications.get(&tid)
                && matches!(publication.media_kind, crate::MediaKind::Video)
            {
                new_video_publishers.insert(publication.publisher.0.clone());
            }
        }

        let revision = rs.meeting.next_revision();
        Self::recompute_meeting_plan(rs);
        drop(g);

        for publisher_id in new_video_publishers {
            self.schedule_meeting_keyframe_warmup(room.clone(), publisher_id);
        }

        Ok(revision)
    }

    pub fn meeting_unsubscribe(
        &self,
        room: RoomId,
        participant_id: &str,
        track_ids: Vec<String>,
    ) -> Result<u64> {
        let mut g = self.inner.write();
        let rs = g
            .rooms
            .get_mut(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let pid = ParticipantId(participant_id.to_string());
        if !rs.meeting.participants.contains_key(&pid) {
            return Err(anyhow::anyhow!("participant not joined"));
        }

        let subs = rs.meeting.subscription_requests.entry(pid).or_default();
        for raw_id in track_ids {
            subs.remove(&crate::TrackId(raw_id));
        }

        let revision = rs.meeting.next_revision();
        Self::recompute_meeting_plan(rs);
        Ok(revision)
    }

    pub fn meeting_list_publications(&self, room: RoomId) -> Result<Vec<MeetingPublicationInfo>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        Ok(rs
            .meeting
            .publications
            .values()
            .map(|p| MeetingPublicationInfo {
                track_id: p.track_id.0.clone(),
                publisher_id: p.publisher.0.clone(),
                media_kind: p.media_kind,
                mid: p.mid.clone(),
            })
            .collect())
    }

    pub fn meeting_list_participants(&self, room: RoomId) -> Result<Vec<MeetingParticipantInfo>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let mut out = rs
            .meeting
            .participants
            .values()
            .map(|p| MeetingParticipantInfo {
                participant_id: p.id.0.clone(),
                display_name: p.display_name.clone(),
            })
            .collect::<Vec<_>>();
        out.sort_by(|a, b| a.participant_id.cmp(&b.participant_id));
        Ok(out)
    }

    pub fn meeting_list_subscription_requests(
        &self,
        room: RoomId,
        participant_id: &str,
    ) -> Result<Vec<String>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let pid = ParticipantId(participant_id.to_string());
        if !rs.meeting.participants.contains_key(&pid) {
            return Err(anyhow::anyhow!("participant not joined"));
        }

        let mut out = rs
            .meeting
            .subscription_requests
            .get(&pid)
            .map(|s| s.iter().map(|id| id.0.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }

    pub fn meeting_list_effective_subscriptions(
        &self,
        room: RoomId,
        participant_id: &str,
    ) -> Result<Vec<String>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let pid = ParticipantId(participant_id.to_string());
        if !rs.meeting.participants.contains_key(&pid) {
            return Err(anyhow::anyhow!("participant not joined"));
        }

        Ok(rs
            .meeting_plan
            .effective_track_ids_for(&pid)
            .into_iter()
            .map(|id| id.0)
            .collect())
    }

    pub fn meeting_list_streams(&self, room: RoomId) -> Result<Vec<MeetingStreamInfo>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        let mut out = rs
            .meeting_streams
            .values()
            .flat_map(|by_ssrc| by_ssrc.values().cloned())
            .collect::<Vec<_>>();
        out.sort_by(|a, b| {
            a.track_id
                .cmp(&b.track_id)
                .then(a.ssrc.cmp(&b.ssrc))
                .then(a.encoding_id.cmp(&b.encoding_id))
        });
        Ok(out)
    }

    pub async fn meeting_leave(&self, room: RoomId, participant_id: &str) -> Result<()> {
        let participant = {
            let mut g = self.inner.write();
            let rs = g
                .rooms
                .get_mut(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_meeting_mode(rs)?;

            let pid = ParticipantId(participant_id.to_string());
            let published_track_ids = rs
                .meeting
                .publications
                .iter()
                .filter_map(|(track_id, publication)| {
                    if publication.publisher == pid {
                        Some(track_id.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            rs.meeting.participants.remove(&pid);
            rs.meeting.subscription_requests.remove(&pid);
            for subscriptions in rs.meeting.subscription_requests.values_mut() {
                for track_id in &published_track_ids {
                    subscriptions.remove(track_id);
                }
            }
            rs.meeting.publications.retain(|_, p| p.publisher != pid);
            for track_id in &published_track_ids {
                rs.meeting_streams.remove(track_id);
            }
            rs.meeting.next_revision();
            Self::recompute_meeting_plan(rs);
            rs.meeting_participants.remove(participant_id)
        };

        if let Some(participant) = participant {
            participant.peer.stop().await?;
        }
        Ok(())
    }

    pub async fn meeting_sdp_offer(
        &self,
        room: RoomId,
        participant_id: &str,
        revision: u64,
        offer_sdp: String,
    ) -> Result<MeetingSdpResponse> {
        let need_create = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_meeting_mode(rs)?;
            let pid = ParticipantId(participant_id.to_string());
            if !rs.meeting.participants.contains_key(&pid) {
                return Err(anyhow::anyhow!("participant not joined"));
            }
            if revision > rs.meeting.revision {
                return Err(anyhow::anyhow!(
                    "invalid future revision: got {revision}, current {}",
                    rs.meeting.revision
                ));
            }
            !rs.meeting_participants.contains_key(participant_id)
        };

        if need_create {
            let peer = PeerSession::new(
                self.gst.clone(),
                participant_id.to_string(),
                PeerRole::MeetingParticipant,
                None,
            )
            .await?;
            peer.start().await?;
            let tap = peer.install_rtp_tap().await?;
            let injector = peer.install_rtp_injector(Vec::new()).await?;

            let ice = IceQueue::new();
            start_ice_collector(peer.ice_subscribe(), ice.clone());

            let rooms = self.clone();
            let room_for_tap = room.clone();
            let participant_for_tap = participant_id.to_string();
            tokio::spawn(async move {
                let mut rx = tap.rx;
                while let Some(pkt) = rx.recv().await {
                    rooms.forward_meeting_rtp(&room_for_tap, &participant_for_tap, pkt);
                }
            });

            let mut g = self.inner.write();
            let rs = g
                .rooms
                .get_mut(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_meeting_mode(rs)?;
            rs.meeting_participants
                .entry(participant_id.to_string())
                .or_insert(MeetingParticipantSession {
                    id: participant_id.to_string(),
                    peer,
                    ice,
                    injector_tx: injector.tx.clone(),
                    ssrc_to_mid: HashMap::new(),
                    mid_routing: HashMap::new(),
                });
        }

        let offer_ssrc_to_mid = parse_offer_ssrc_to_mid(&offer_sdp);
        let offer_media_mids = parse_offer_media_mids(&offer_sdp);
        let offer_mid_routing = parse_offer_mid_routing(&offer_sdp);

        {
            let mut g = self.inner.write();
            let rs = g
                .rooms
                .get_mut(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_meeting_mode(rs)?;

            if let Some(session) = rs.meeting_participants.get_mut(participant_id) {
                session.ssrc_to_mid = offer_ssrc_to_mid;
                session.mid_routing = offer_mid_routing;
            }

            for (media_kind, mid) in offer_media_mids {
                let mut matching_tracks = rs
                    .meeting
                    .publications
                    .values_mut()
                    .filter(|publication| {
                        publication.publisher.0 == participant_id
                            && publication.media_kind == media_kind
                            && publication.mid.is_none()
                    })
                    .collect::<Vec<_>>();
                if matching_tracks.len() == 1 {
                    matching_tracks[0].mid = Some(mid);
                }
            }

            Self::recompute_meeting_plan(rs);
        }

        let peer = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_meeting_mode(rs)?;
            rs.meeting_participants
                .get(participant_id)
                .ok_or_else(|| anyhow::anyhow!("meeting participant session not found"))?
                .peer
                .clone()
        };

        let answer_sdp = peer.negotiate_as_answerer(offer_sdp).await?;
        peer.play().await?;

        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;

        Ok(MeetingSdpResponse {
            answer_sdp,
            revision: rs.meeting.revision,
        })
    }

    pub async fn meeting_trickle(
        &self,
        room: RoomId,
        participant_id: &str,
        mline: u32,
        cand: String,
    ) -> Result<()> {
        let peer = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_meeting_mode(rs)?;
            rs.meeting_participants
                .get(participant_id)
                .ok_or_else(|| anyhow::anyhow!("meeting participant session not found"))?
                .peer
                .clone()
        };
        peer.add_ice_candidate(mline, cand).await
    }

    pub fn meeting_drain_ice(
        &self,
        room: RoomId,
        participant_id: &str,
        max: usize,
    ) -> Result<Vec<media::IceCandidate>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_meeting_mode(rs)?;
        let participant = rs
            .meeting_participants
            .get(participant_id)
            .ok_or_else(|| anyhow::anyhow!("meeting participant session not found"))?;
        Ok(participant.ice.drain(max))
    }

    fn forward_meeting_rtp(&self, room: &RoomId, publisher_id: &str, pkt: RtpPacket) {
        let kind = match media_kind_from_packet(&pkt) {
            Some(v) => v,
            None => return,
        };

        let (track_id, targets) = {
            let mut g = self.inner.write();
            let Some(rs) = g.rooms.get_mut(room) else {
                return;
            };
            if !matches!(rs.mode, RoomMode::Meeting) {
                return;
            }

            let publisher = ParticipantId(publisher_id.to_string());
            let mapped_mid = pkt.meta.ssrc.and_then(|ssrc| {
                rs.meeting_participants
                    .get(publisher_id)
                    .and_then(|session| session.ssrc_to_mid.get(&ssrc).cloned())
            });
            let track_id = rs.meeting.publications.iter().find_map(|(tid, p)| {
                if p.publisher != publisher || p.media_kind != kind {
                    return None;
                }
                if let Some(mid) = mapped_mid.as_ref() {
                    return (p.mid.as_ref() == Some(mid)).then_some(tid.clone());
                }
                Some(tid.clone())
            });
            let Some(track_id) = track_id else {
                let key = format!("no-track:{publisher_id}:{kind:?}");
                if rs.meeting_route_log_once.insert(key) {
                    tracing::info!(
                        publisher_id = %publisher_id,
                        media_kind = ?kind,
                        "meeting RTP arrived but no published track matched this publisher+kind"
                    );
                }
                return;
            };

            let rid = mapped_mid
                .as_ref()
                .and_then(|mid| {
                    rs.meeting_participants
                        .get(publisher_id)
                        .and_then(|session| session.mid_routing.get(mid))
                        .and_then(|routing| parse_rtp_rid(&pkt.data, routing.rid_ext_id?))
                });
            let packet_encoding_id = rid
                .clone()
                .or_else(|| pkt.meta.ssrc.map(|ssrc| format!("ssrc:{ssrc}")));

            if let Some(ssrc) = pkt.meta.ssrc {
                let publisher_id = publisher_id.to_string();
                let media_kind = kind;
                let track_id_for_log = track_id.0.clone();
                let spatial_layer = mapped_mid.as_ref().and_then(|mid| {
                    rs.meeting_participants
                        .get(&publisher_id)
                        .and_then(|session| session.mid_routing.get(mid))
                        .and_then(|routing| {
                            let rid = rid.as_ref()?;
                            routing
                                .send_rids
                                .iter()
                                .position(|candidate| candidate == rid)
                                .map(|idx| idx as u8)
                        })
                });
                let entry = rs.meeting_streams.entry(track_id.clone()).or_default();
                entry.entry(ssrc).or_insert_with(|| {
                    let encoding_id = rid
                        .clone()
                        .unwrap_or_else(|| format!("ssrc:{ssrc}"));
                    tracing::info!(
                        publisher_id = %publisher_id,
                        track_id = %track_id_for_log,
                        media_kind = ?media_kind,
                        ssrc,
                        encoding_id = %encoding_id,
                        "observed meeting RTP stream"
                    );
                    MeetingStreamInfo {
                        track_id: track_id_for_log,
                        publisher_id,
                        media_kind,
                        ssrc,
                        encoding_id,
                        mid: mapped_mid.clone(),
                        rid,
                        spatial_layer,
                    }
                });
                hydrate_meeting_graph(&mut rs.meeting_graph, &rs.meeting_streams);
            }

            let targets = rs
                .meeting_graph
                .for_track(&track_id)
                .iter()
                .filter_map(|forwarding| {
                    if forwarding.subscriber_id.0 == publisher_id {
                        return None;
                    }
                    let allowed = match (&forwarding.kind, kind) {
                        (ForwardingKind::Audio, crate::MediaKind::Audio) => true,
                        (ForwardingKind::Video(video), crate::MediaKind::Video) => {
                            video_forwarding_allows_packet(
                                &pkt,
                                packet_encoding_id.as_deref(),
                                video,
                            )
                        }
                        _ => false,
                    };
                    if !allowed {
                        return None;
                    }
                    rs.meeting_participants
                        .get(&forwarding.subscriber_id.0)
                        .map(|sess| (forwarding.subscriber_id.0.clone(), sess.injector_tx.clone()))
                })
                .collect::<Vec<_>>();

            if targets.is_empty() {
                let key = format!("no-target:{publisher_id}:{kind:?}:{}", track_id.0);
                if rs.meeting_route_log_once.insert(key) {
                    tracing::info!(
                        publisher_id = %publisher_id,
                        track_id = %track_id.0,
                        media_kind = ?kind,
                        "meeting RTP arrived but no subscribers currently selected this track"
                    );
                }
            } else {
                for (participant_id, _) in &targets {
                    let key = format!(
                        "route:{publisher_id}:{participant_id}:{kind:?}:{}",
                        track_id.0
                    );
                    if rs.meeting_route_log_once.insert(key) {
                        tracing::info!(
                            publisher_id = %publisher_id,
                            subscriber_id = %participant_id,
                            track_id = %track_id.0,
                            media_kind = ?kind,
                            "meeting RTP route became active"
                        );
                    }
                }
            }

            (track_id, targets)
        };

        for (participant_id, tx) in targets {
            match tx.try_send(pkt.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    tracing::warn!(
                        publisher_id = %publisher_id,
                        subscriber_id = %participant_id,
                        track_id = %track_id.0,
                        "dropping meeting RTP packet because subscriber injector queue is full"
                    );
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::debug!(
                        publisher_id = %publisher_id,
                        subscriber_id = %participant_id,
                        track_id = %track_id.0,
                        "meeting subscriber injector channel closed"
                    );
                }
            }
        }
    }

    fn schedule_meeting_keyframe_warmup(&self, room: RoomId, publisher_id: String) {
        let rooms = self.clone();
        tokio::spawn(async move {
            for attempt in 1..=3u8 {
                if let Err(err) = rooms
                    .request_meeting_participant_keyframe(room.clone(), &publisher_id)
                    .await
                {
                    tracing::warn!(
                        publisher_id = %publisher_id,
                        attempt,
                        "meeting keyframe warmup request failed: {err:#}"
                    );
                } else {
                    tracing::info!(
                        publisher_id = %publisher_id,
                        attempt,
                        "meeting keyframe warmup requested"
                    );
                }
                sleep(Duration::from_millis(700)).await;
            }
        });
    }

    async fn request_meeting_participant_keyframe(
        &self,
        room: RoomId,
        participant_id: &str,
    ) -> Result<()> {
        let peer = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_meeting_mode(rs)?;
            rs.meeting_participants
                .get(participant_id)
                .ok_or_else(|| anyhow::anyhow!("meeting publisher session not found"))?
                .peer
                .clone()
        };

        peer.request_keyframe().await
    }

    async fn request_publisher_keyframe(&self, room: RoomId) -> Result<()> {
        let publisher = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_broadcast_mode(rs)?;
            rs.publisher
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no publisher"))?
                .peer
                .clone()
        };

        publisher.request_keyframe().await
    }

    /// POST /whip/:room  (offer SDP -> answer SDP)
    pub async fn whip_publish(&self, room: RoomId, offer_sdp: String) -> Result<SdpResponse> {
        let pub_id = Uuid::new_v4().to_string();

        let peer = PeerSession::new(
            self.gst.clone(),
            pub_id.clone(),
            PeerRole::WhipPublisher,
            None,
        )
        .await?;

        peer.start().await?;
        let answer = peer.negotiate_as_answerer(offer_sdp).await?;

        // Install RTP tap (audio + video will appear as src_%u pads)
        let tap = peer.install_rtp_tap().await?;
        peer.play().await?;
        let ice = IceQueue::new();
        start_ice_collector(peer.ice_subscribe(), ice.clone());

        let (router, old_publisher) = {
            let mut g = self.inner.write();
            let rs = Self::room_mut(&mut g, &room);
            Self::require_broadcast_mode(rs)?;

            let router = BroadcastRouter::new();
            for (sub_id, tx) in rs.router.subscriber_entries() {
                router.add_sub(sub_id, tx);
            }

            let old_publisher = rs.publisher.take();
            rs.router = router.clone();
            // v1 policy: replace existing publisher
            rs.publisher = Some(PublisherSession {
                id: pub_id.clone(),
                peer,
                ice,
            });
            (router, old_publisher)
        };

        if let Some(old_publisher) = old_publisher {
            old_publisher.peer.stop().await?;
        }

        tokio::spawn(async move {
            let mut rx = tap.rx;
            while let Some(pkt) = rx.recv().await {
                router.fanout(pkt);
            }
        });

        Ok(SdpResponse {
            answer_sdp: answer,
            location: format!("/whip/{}/{}", room.0, pub_id),
        })
    }

    /// PATCH /whip/:room/:pub  (client trickle ICE -> server)
    pub async fn whip_trickle(
        &self,
        room: RoomId,
        pub_id: &str,
        mline: u32,
        cand: String,
    ) -> Result<()> {
        let pub_peer = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_broadcast_mode(rs)?;
            let pub_sess = rs
                .publisher
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no publisher"))?;
            if pub_sess.id != pub_id {
                return Err(anyhow::anyhow!("publisher id mismatch"));
            }
            pub_sess.peer.clone()
        };
        pub_peer.add_ice_candidate(mline, cand).await
    }

    /// GET /whip/:room/:pub/ice  (server ICE -> client)
    pub fn whip_drain_ice(
        &self,
        room: RoomId,
        pub_id: &str,
        max: usize,
    ) -> Result<Vec<media::IceCandidate>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_broadcast_mode(rs)?;
        let pub_sess = rs
            .publisher
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no publisher"))?;
        if pub_sess.id != pub_id {
            return Err(anyhow::anyhow!("publisher id mismatch"));
        }
        Ok(pub_sess.ice.drain(max))
    }

    pub async fn whip_stop(&self, room: RoomId, pub_id: &str) -> Result<()> {
        let pub_sess = {
            let mut g = self.inner.write();
            let rs = g
                .rooms
                .get_mut(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_broadcast_mode(rs)?;

            let cur = rs
                .publisher
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no publisher"))?;
            if cur.id != pub_id {
                return Err(anyhow::anyhow!("publisher id mismatch"));
            }

            rs.publisher.take().unwrap()
        };

        pub_sess.peer.stop().await?;
        Ok(())
    }

    /// POST /whep/:room  (offer SDP -> answer SDP)
    pub async fn whep_subscribe(&self, room: RoomId, offer_sdp: String) -> Result<SdpResponse> {
        let sub_id = Uuid::new_v4().to_string();

        // v1 policy: require publisher exists
        let stream_infos = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_broadcast_mode(rs)?;
            rs.publisher
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no publisher"))?;
            rs.router.stream_infos()
        };

        if stream_infos.is_empty() {
            return Err(anyhow::anyhow!(
                "publisher has not produced RTP yet; try subscribing after media is flowing"
            ));
        }

        // v1 policy: require publisher exists
        {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_broadcast_mode(rs)?;
            rs.publisher
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no publisher"))?;
        }

        let peer = PeerSession::new(
            self.gst.clone(),
            sub_id.clone(),
            PeerRole::WhepSubscriber,
            Some(Arc::new(RoomPeerEvents {
                rooms: self.clone(),
                room: room.clone(),
                tokio_handle: Handle::current(),
            })),
        )
        .await?;

        peer.set_whep_streams(stream_infos);
        peer.start().await?;
        let answer = peer.negotiate_as_answerer(offer_sdp).await?;
        let injector = peer
            .take_whep_injector()
            .ok_or_else(|| anyhow::anyhow!("whep injector was not created during negotiation"))?;
        peer.play().await?;

        // Register this subscriber in the room router
        {
            let mut g = self.inner.write();
            let rs = Self::room_mut(&mut g, &room);
            Self::require_broadcast_mode(rs)?;
            rs.router.add_sub(sub_id.clone(), injector.tx.clone());
        }

        // Prompt a fresh keyframe as soon as a subscriber joins so startup
        // doesn't depend on the publisher's natural GOP interval.
        if let Err(err) = self.request_publisher_keyframe(room.clone()).await {
            tracing::warn!("failed to request keyframe for new subscriber: {err:#}");
        }

        let ice = IceQueue::new();
        start_ice_collector(peer.ice_subscribe(), ice.clone());

        {
            let mut g = self.inner.write();
            let rs = Self::room_mut(&mut g, &room);
            Self::require_broadcast_mode(rs)?;
            rs.subscribers.insert(
                sub_id.clone(),
                SubscriberSession {
                    id: sub_id.clone(),
                    peer,
                    ice,
                },
            );
        }

        Ok(SdpResponse {
            answer_sdp: answer,
            location: format!("/whep/{}/{}", room.0, sub_id),
        })
    }

    /// PATCH /whep/:room/:sub  (client trickle ICE -> server)
    pub async fn whep_trickle(
        &self,
        room: RoomId,
        sub_id: &str,
        mline: u32,
        cand: String,
    ) -> Result<()> {
        let sub_peer = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_broadcast_mode(rs)?;
            let sub = rs
                .subscribers
                .get(sub_id)
                .ok_or_else(|| anyhow::anyhow!("subscriber not found"))?;
            sub.peer.clone()
        };

        sub_peer.add_ice_candidate(mline, cand).await
    }

    /// GET /whep/:room/:sub/ice  (server ICE -> client)
    pub fn whep_drain_ice(
        &self,
        room: RoomId,
        sub_id: &str,
        max: usize,
    ) -> Result<Vec<media::IceCandidate>> {
        let g = self.inner.read();
        let rs = g
            .rooms
            .get(&room)
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        Self::require_broadcast_mode(rs)?;
        let sub = rs
            .subscribers
            .get(sub_id)
            .ok_or_else(|| anyhow::anyhow!("subscriber not found"))?;
        Ok(sub.ice.drain(max))
    }

    pub async fn whep_stop(&self, room: RoomId, sub_id: &str) -> Result<()> {
        // take subscriber out of state first (short lock)
        let sub = {
            let mut g = self.inner.write();
            let rs = g
                .rooms
                .get_mut(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            Self::require_broadcast_mode(rs)?;

            rs.router.remove_sub(sub_id);

            rs.subscribers
                .remove(sub_id)
                .ok_or_else(|| anyhow::anyhow!("subscriber not found"))?
        };

        // stop media outside lock
        sub.peer.stop().await?;
        Ok(())
    }
}

fn media_kind_from_packet(pkt: &RtpPacket) -> Option<crate::MediaKind> {
    pkt.caps
        .structure(0)
        .and_then(|s| s.get_optional::<String>("media").ok().flatten())
        .and_then(|m| match m.as_str() {
            "audio" => Some(crate::MediaKind::Audio),
            "video" => Some(crate::MediaKind::Video),
            _ => None,
        })
}

fn video_forwarding_allows_packet(
    _pkt: &RtpPacket,
    packet_encoding_id: Option<&str>,
    _video: &VideoForwarding,
) -> bool {
    if let Some(preferred_encoding_id) = _video.preferred_encoding_id.as_ref() {
        if let Some(packet_encoding_id) = packet_encoding_id {
            if packet_encoding_id != preferred_encoding_id {
                return false;
            }
        }
    }
    let Some(max_temporal_layer) = _video.temporal_layer else {
        return true;
    };
    let Some(packet_temporal_layer) = _pkt.meta.temporal_layer else {
        // Packets without exposed temporal metadata still pass through for now.
        return true;
    };
    packet_temporal_layer <= max_temporal_layer
}

fn hydrate_meeting_graph(
    graph: &mut ForwardingGraph,
    meeting_streams: &HashMap<crate::TrackId, HashMap<u32, MeetingStreamInfo>>,
) {
    for (track_id, forwardings) in &mut graph.track_subscribers {
        let Some(streams_for_track) = meeting_streams.get(track_id) else {
            continue;
        };
        for forwarding in forwardings {
            if let ForwardingKind::Video(video) = &mut forwarding.kind {
                video.preferred_encoding_id =
                    select_preferred_encoding_id(video, streams_for_track);
            }
        }
    }
}

fn select_preferred_encoding_id(
    video: &VideoForwarding,
    streams_for_track: &HashMap<u32, MeetingStreamInfo>,
) -> Option<String> {
    let mut candidates = streams_for_track.values().collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        a.spatial_layer
            .unwrap_or(u8::MAX)
            .cmp(&b.spatial_layer.unwrap_or(u8::MAX))
            .then(a.encoding_id.cmp(&b.encoding_id))
    });

    let target_spatial = video.spatial_layer;
    candidates
        .iter()
        .rev()
        .find(|stream| match (stream.spatial_layer, target_spatial) {
            (Some(layer), Some(target)) => layer <= target,
            (Some(_), None) => true,
            (None, _) => false,
        })
        .or_else(|| candidates.iter().find(|stream| stream.spatial_layer.is_none()))
        .map(|stream| stream.encoding_id.clone())
}

fn parse_offer_ssrc_to_mid(sdp: &str) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let mut current_mid = None::<String>;

    for line in sdp.lines() {
        if line.starts_with("m=") {
            current_mid = None;
        } else if let Some(rest) = line.strip_prefix("a=mid:") {
            current_mid = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("a=ssrc:") {
            let Some(mid) = current_mid.as_ref() else {
                continue;
            };
            let Some((ssrc_txt, _attr)) = rest.split_once(' ') else {
                continue;
            };
            let Ok(ssrc) = ssrc_txt.parse::<u32>() else {
                continue;
            };
            out.insert(ssrc, mid.clone());
        }
    }

    out
}

fn parse_offer_media_mids(sdp: &str) -> Vec<(crate::MediaKind, String)> {
    let mut out = Vec::new();
    let mut current_media = None::<crate::MediaKind>;

    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=") {
            current_media = if rest.starts_with("audio ") {
                Some(crate::MediaKind::Audio)
            } else if rest.starts_with("video ") {
                Some(crate::MediaKind::Video)
            } else {
                None
            };
        } else if let Some(rest) = line.strip_prefix("a=mid:") {
            if let Some(media_kind) = current_media {
                out.push((media_kind, rest.trim().to_string()));
            }
        }
    }

    out
}

fn parse_offer_mid_routing(sdp: &str) -> HashMap<String, MidRoutingInfo> {
    let mut out: HashMap<String, MidRoutingInfo> = HashMap::new();
    let mut current_mid = None::<String>;
    let mut current_rids = Vec::<String>::new();

    for line in sdp.lines() {
        if line.starts_with("m=") {
            current_mid = None;
            current_rids.clear();
        } else if let Some(rest) = line.strip_prefix("a=mid:") {
            let mid = rest.trim().to_string();
            current_mid = Some(mid.clone());
            out.entry(mid).or_default();
        } else if let Some(rest) = line.strip_prefix("a=extmap:") {
            let Some(mid) = current_mid.as_ref() else {
                continue;
            };
            let Some((id_txt, uri_and_rest)) = rest.split_once(' ') else {
                continue;
            };
            let Ok(id) = id_txt.split('/').next().unwrap_or("").parse::<u8>() else {
                continue;
            };
            let uri = uri_and_rest.split_whitespace().next().unwrap_or("");
            if uri == "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id" {
                out.entry(mid.clone()).or_default().rid_ext_id = Some(id);
            }
        } else if let Some(rest) = line.strip_prefix("a=rid:") {
            let Some(mid) = current_mid.as_ref() else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let Some(rid) = parts.next() else {
                continue;
            };
            let Some(direction) = parts.next() else {
                continue;
            };
            if direction == "send" {
                let rid = rid.to_string();
                if !current_rids.contains(&rid) {
                    current_rids.push(rid.clone());
                }
                let routing = out.entry(mid.clone()).or_default();
                if !routing.send_rids.contains(&rid) {
                    routing.send_rids.push(rid);
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=simulcast:") {
            let Some(mid) = current_mid.as_ref() else {
                continue;
            };
            let Some(send_part) = rest
                .split_whitespace()
                .find_map(|part| part.strip_prefix("send "))
                .or_else(|| rest.strip_prefix("send "))
            else {
                continue;
            };

            let ordered = send_part
                .split(';')
                .flat_map(|group| group.split(','))
                .filter_map(|token| {
                    let token = token.trim().trim_start_matches('~');
                    (!token.is_empty()).then_some(token.to_string())
                })
                .collect::<Vec<_>>();
            if !ordered.is_empty() {
                out.entry(mid.clone()).or_default().send_rids = ordered;
            }
        }
    }

    out
}

fn parse_rtp_rid(pkt: &[u8], ext_id: u8) -> Option<String> {
    let data = parse_rtp_header_extension(pkt, ext_id)?;
    std::str::from_utf8(data).ok().map(ToString::to_string)
}

fn parse_rtp_header_extension<'a>(pkt: &'a [u8], ext_id: u8) -> Option<&'a [u8]> {
    if pkt.len() < 12 || ext_id == 0 || ext_id > 14 {
        return None;
    }
    let v = pkt[0] >> 6;
    if v != 2 {
        return None;
    }
    let cc = (pkt[0] & 0x0f) as usize;
    let has_ext = (pkt[0] & 0x10) != 0;
    let mut off = 12usize.checked_add(cc.checked_mul(4)?)?;
    if pkt.len() < off || !has_ext || pkt.len() < off + 4 {
        return None;
    }
    let profile = u16::from_be_bytes([pkt[off], pkt[off + 1]]);
    let ext_words = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
    off += 4;
    let ext_end = off.checked_add(ext_words.checked_mul(4)?)?;
    if pkt.len() < ext_end {
        return None;
    }
    if profile != 0xBEDE {
        return None;
    }

    let mut cursor = off;
    while cursor < ext_end {
        let b = pkt[cursor];
        cursor += 1;
        if b == 0 {
            continue;
        }
        let id = b >> 4;
        let len = ((b & 0x0f) as usize) + 1;
        if cursor.checked_add(len)? > ext_end {
            return None;
        }
        let data = &pkt[cursor..cursor + len];
        cursor += len;
        if id == ext_id {
            return Some(data);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use gstreamer as gst;

    fn manager() -> RoomManager {
        let gst = GstRuntime::init().expect("gstreamer runtime must initialize");
        RoomManager::new(gst)
    }

    fn publish_tracks() -> Vec<MeetingPublishTrack> {
        vec![
            MeetingPublishTrack {
                track_id: "alice-audio".to_string(),
                media_kind: crate::MediaKind::Audio,
                mid: None,
            },
            MeetingPublishTrack {
                track_id: "alice-video-1".to_string(),
                media_kind: crate::MediaKind::Video,
                mid: None,
            },
            MeetingPublishTrack {
                track_id: "alice-video-2".to_string(),
                media_kind: crate::MediaKind::Video,
                mid: None,
            },
            MeetingPublishTrack {
                track_id: "alice-video-3".to_string(),
                media_kind: crate::MediaKind::Video,
                mid: None,
            },
            MeetingPublishTrack {
                track_id: "alice-video-4".to_string(),
                media_kind: crate::MediaKind::Video,
                mid: None,
            },
        ]
    }

    fn join_publish_and_subscribe(manager: &RoomManager, room: &RoomId) {
        manager
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        manager
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");
        manager
            .meeting_publish_tracks(room.clone(), "alice", publish_tracks())
            .expect("alice publishes");
        manager
            .meeting_subscribe(
                room.clone(),
                "bob",
                vec![
                    "alice-audio".to_string(),
                    "alice-video-1".to_string(),
                    "alice-video-2".to_string(),
                    "alice-video-3".to_string(),
                    "alice-video-4".to_string(),
                ],
            )
            .expect("bob subscribes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn room_policy_mode_recomputes_effective_subscriptions() {
        let manager = manager();
        let room = RoomId("room-policy".to_string());
        join_publish_and_subscribe(&manager, &room);

        let requested = manager
            .meeting_list_subscription_requests(room.clone(), "bob")
            .expect("requested tracks");
        let effective_standard = manager
            .meeting_list_effective_subscriptions(room.clone(), "bob")
            .expect("effective tracks in standard mode");

        let revision = manager
            .meeting_set_policy_mode(room.clone(), MeetingPolicyMode::LowBandwidth)
            .expect("set low bandwidth policy");
        let effective_low_bandwidth = manager
            .meeting_list_effective_subscriptions(room.clone(), "bob")
            .expect("effective tracks in low bandwidth mode");

        assert_eq!(
            manager.meeting_policy_mode(room.clone()).unwrap(),
            MeetingPolicyMode::LowBandwidth
        );
        assert!(revision > 0);
        assert_eq!(requested.len(), 5);
        assert_eq!(effective_standard.len(), 5);
        assert_eq!(effective_low_bandwidth.len(), 4);
        assert!(requested.contains(&"alice-video-4".to_string()));
        assert!(!effective_low_bandwidth.contains(&"alice-video-4".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn meeting_leave_prunes_requests_and_effective_tracks() {
        let manager = manager();
        let room = RoomId("room-leave".to_string());
        join_publish_and_subscribe(&manager, &room);

        manager
            .meeting_leave(room.clone(), "alice")
            .await
            .expect("alice leaves");

        let requested = manager
            .meeting_list_subscription_requests(room.clone(), "bob")
            .expect("bob requested tracks after alice leaves");
        let effective = manager
            .meeting_list_effective_subscriptions(room.clone(), "bob")
            .expect("bob effective tracks after alice leaves");
        let publications = manager
            .meeting_list_publications(room.clone())
            .expect("publications after alice leaves");

        assert!(requested.is_empty());
        assert!(effective.is_empty());
        assert!(publications.is_empty());
    }

    #[test]
    fn observed_streams_are_tracked_by_ssrc() {
        let manager = manager();
        let room = RoomId("room-streams".to_string());

        manager
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        manager
            .meeting_publish_tracks(room.clone(), "alice", publish_tracks())
            .expect("alice publishes");

        let pkt = RtpPacket {
            pad_name: "src_0".to_string(),
            caps: gst::Caps::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "VP8")
                .build(),
            data: Bytes::from_static(&[]),
            meta: media::RtpMeta {
                ssrc: Some(0x1122_3344),
                sequence_number: Some(1),
                timestamp: Some(1),
                temporal_layer: Some(0),
            },
            pts: None,
            dts: None,
            duration: None,
        };

        manager.forward_meeting_rtp(&room, "alice", pkt);

        let streams = manager
            .meeting_list_streams(room)
            .expect("meeting streams should be listable");
        assert_eq!(streams.len(), 1);
        assert!(streams[0].track_id.starts_with("alice-video-"));
        assert_eq!(streams[0].publisher_id, "alice");
        assert_eq!(streams[0].ssrc, 0x1122_3344);
        assert_eq!(streams[0].encoding_id, "ssrc:287454020");
        assert_eq!(streams[0].mid, None);
        assert_eq!(streams[0].rid, None);
        assert_eq!(streams[0].spatial_layer, None);
    }

    #[test]
    fn parses_offer_ssrc_to_mid_map() {
        let sdp = "\
v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:0\r\n\
a=ssrc:1234 cname:test\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:1\r\n\
a=ssrc:5678 cname:test\r\n";

        let map = parse_offer_ssrc_to_mid(sdp);
        assert_eq!(map.get(&1234).map(String::as_str), Some("0"));
        assert_eq!(map.get(&5678).map(String::as_str), Some("1"));
    }

    #[test]
    fn parses_offer_media_mids() {
        let sdp = "\
v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:audio-mid\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:video-mid\r\n";

        let mids = parse_offer_media_mids(sdp);
        assert_eq!(
            mids,
            vec![
                (crate::MediaKind::Audio, "audio-mid".to_string()),
                (crate::MediaKind::Video, "video-mid".to_string()),
            ]
        );
    }

    #[test]
    fn parses_offer_mid_routing_info() {
        let sdp = "\
v=0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:video-mid\r\n\
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id\r\n\
a=rid:f send\r\n\
a=rid:h send\r\n\
a=simulcast:send f;h\r\n";

        let routing = parse_offer_mid_routing(sdp);
        let video = routing.get("video-mid").expect("video mid must be parsed");
        assert_eq!(video.rid_ext_id, Some(4));
        assert_eq!(video.send_rids, vec!["f".to_string(), "h".to_string()]);
    }

    #[test]
    fn parses_rtp_rid_from_one_byte_header_extension() {
        let pkt = [
            0x90, 0x60, 0x00, 0x01, 0, 0, 0, 1, 0, 0, 0, 1, // RTP header, X=1
            0xBE, 0xDE, 0x00, 0x01, // one-byte extension, 1 word
            0x30, b'h', 0x00, 0x00, // id=3, len=1 byte
        ];

        assert_eq!(parse_rtp_rid(&pkt, 3), Some("h".to_string()));
    }
}
