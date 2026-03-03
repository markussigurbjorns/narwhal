use crate::BroadcastRouter;
use crate::{IceQueue, PublisherSession, SubscriberSession, start_ice_collector};
use anyhow::Result;
use media::{GstRuntime, PeerEvents, PeerRole, PeerSession, PeerState};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tokio::runtime::Handle;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoomId(pub String);

pub struct SdpResponse {
    pub answer_sdp: String,
    pub location: String,
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
    publisher: Option<PublisherSession>,
    subscribers: HashMap<String, SubscriberSession>,
    router: BroadcastRouter,
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

    fn room_mut<'a>(rooms: &'a mut Rooms, room: &RoomId) -> &'a mut RoomState {
        rooms
            .rooms
            .entry(room.clone())
            .or_insert_with(|| RoomState {
                publisher: None,
                subscribers: HashMap::new(),
                router: BroadcastRouter::new(),
            })
    }

    async fn request_publisher_keyframe(&self, room: RoomId) -> Result<()> {
        let publisher = {
            let g = self.inner.read();
            let rs = g
                .rooms
                .get(&room)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
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
            rs.router.add_sub(sub_id.clone(), injector.tx.clone());
        }

        let ice = IceQueue::new();
        start_ice_collector(peer.ice_subscribe(), ice.clone());

        {
            let mut g = self.inner.write();
            let rs = Self::room_mut(&mut g, &room);
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
