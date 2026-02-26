use crate::{GstRuntime, PeerEvents, PeerRole, PeerSession};
use anyhow::Result;
use std::sync::Arc;

pub struct WhepSession {
    peer: PeerSession,
}

impl WhepSession {
    pub async fn start(
        gst_rt: GstRuntime,
        sub_id: String,
        offer_sdp: String,
        events: Option<Arc<dyn PeerEvents>>,
    ) -> Result<(Self, String)> {
        let peer = PeerSession::new(gst_rt, sub_id, PeerRole::WhepSubscriber, events).await?;
        let answer = peer.negotiate_as_answerer(offer_sdp).await?;
        peer.start().await?;
        Ok((Self { peer }, answer))
    }
}
