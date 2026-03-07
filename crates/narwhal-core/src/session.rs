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
}
