use std::{collections::HashMap, time::Instant};

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
    pub injector_tx: mpsc::Sender<RtpPacket>,
    pub injector_overflow_streak: u32,
    pub subscribed_at: Instant,
    pub first_rtp_forwarded: bool,
    pub first_video_keyframe_forwarded: bool,
}

pub struct MeetingParticipantSession {
    pub id: String,
    pub peer: PeerSession,
    pub ice: IceQueue,
    pub injector_tx: mpsc::Sender<RtpPacket>,
    pub injector_overflow_streak: u32,
    pub ssrc_to_mid: HashMap<u32, String>,
    pub mid_routing: HashMap<String, MidRoutingInfo>,
    pub joined_at: Instant,
    pub first_join_rtp_forwarded: bool,
    pub first_join_video_keyframe_forwarded: bool,
    pub subscribe_started_at: Option<Instant>,
    pub subscribe_keyframe_started_at: Option<Instant>,
}

#[derive(Clone, Debug, Default)]
pub struct MidRoutingInfo {
    pub rid_ext_id: Option<u8>,
    pub send_rids: Vec<String>,
}
