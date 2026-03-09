use std::collections::HashMap;

use media::{PeerSession, RtpPacket};
use tokio::sync::mpsc;

use crate::IceQueue;

pub struct PublisherSession {
    pub id: String,
    pub peer: PeerSession,
    pub ice: IceQueue,
}

pub struct SubscriberSession {
    pub id: String,
    pub peer: PeerSession,
    pub ice: IceQueue,
}

pub struct MeetingParticipantSession {
    pub id: String,
    pub peer: PeerSession,
    pub ice: IceQueue,
    pub injector_tx: mpsc::Sender<RtpPacket>,
    pub ssrc_to_mid: HashMap<u32, String>,
    pub mid_routing: HashMap<String, MidRoutingInfo>,
}

#[derive(Clone, Debug, Default)]
pub struct MidRoutingInfo {
    pub rid_ext_id: Option<u8>,
    pub send_rids: Vec<String>,
}
