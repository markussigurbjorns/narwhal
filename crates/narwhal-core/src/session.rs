use media::PeerSession;

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
