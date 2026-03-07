use crate::BroadcastRouter;
use crate::{
    IceQueue, MeetingParticipantSession, MeetingRoomState, ParticipantId, ParticipantState,
    PublisherSession, RoomMode, SubscriberSession, start_ice_collector,
};
use anyhow::Result;
use media::{GstRuntime, PeerEvents, PeerRole, PeerSession, PeerState, RtpPacket};
use parking_lot::RwLock;
use std::{collections::{HashMap, HashSet}, sync::Arc};
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
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
                meeting_route_log_once: HashSet::new(),
            })
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

        Ok(rs.meeting.next_revision())
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
            for subs in rs.meeting.subscriptions.values_mut() {
                subs.remove(&tid);
            }
        }

        Ok(rs.meeting.next_revision())
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
        let subs = rs.meeting.subscriptions.entry(pid).or_default();
        for tid in wanted {
            subs.insert(tid);
        }

        Ok(rs.meeting.next_revision())
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

        let subs = rs.meeting.subscriptions.entry(pid).or_default();
        for raw_id in track_ids {
            subs.remove(&crate::TrackId(raw_id));
        }

        Ok(rs.meeting.next_revision())
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

    pub fn meeting_list_subscriptions(
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
            .subscriptions
            .get(&pid)
            .map(|s| s.iter().map(|id| id.0.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        out.sort();
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
            rs.meeting.participants.remove(&pid);
            rs.meeting.subscriptions.remove(&pid);
            rs.meeting.publications.retain(|_, p| p.publisher != pid);
            rs.meeting.next_revision();
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
                });
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
            let track_id = rs
                .meeting
                .publications
                .iter()
                .find_map(|(tid, p)| {
                    if p.publisher == publisher && p.media_kind == kind {
                        Some(tid.clone())
                    } else {
                        None
                    }
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

            let targets = rs
                .meeting
                .subscriptions
                .iter()
                .filter_map(|(participant, tracks)| {
                    if !tracks.contains(&track_id) || participant.0 == publisher_id {
                        return None;
                    }
                    rs.meeting_participants
                        .get(&participant.0)
                        .map(|sess| (participant.0.clone(), sess.injector_tx.clone()))
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
