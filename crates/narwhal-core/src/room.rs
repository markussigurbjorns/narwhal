use crate::{IceQueue, PublisherSession, SubscriberSession, start_ice_collector};
use anyhow::Result;
use media::{GstRuntime, PeerRole, PeerSession};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
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
            })
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

        let answer = peer.negotiate_as_answerer(offer_sdp).await?;
        peer.start().await?;

        let ice = IceQueue::new();
        start_ice_collector(peer.ice_subscribe(), ice.clone());

        {
            let mut g = self.inner.write();
            let rs = Self::room_mut(&mut g, &room);
            // v1 policy: replace existing publisher
            rs.publisher = Some(PublisherSession {
                id: pub_id.clone(),
                peer,
                ice,
            });
        }

        Ok(SdpResponse {
            answer_sdp: answer,
            location: format!("/whip/{}/{}", room.0, pub_id),
        })
    }

    /// PATCH /whip/:room/:pub  (client trickle ICE -> server)
    
}
