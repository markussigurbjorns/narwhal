// crates/media/src/peer.rs
//
// A reusable WebRTC peer wrapper around GStreamer's webrtcbin.
// This is the shared building block for:
//   - WHIP publisher
//   - WHEP subscriber
//   - Meeting participants
//
// It handles:
//   - set remote SDP offer
//   - create local SDP answer (and set-local-description)
//   - add ICE candidate (trickle from client)
//   - emits outgoing ICE candidates via a callback
//
use anyhow::Result;
use glib::object::Cast;
use glib::{ControlFlow, object::ObjectExt};
use gstreamer::bus::BusWatchGuard;
use gstreamer::prelude::GstObjectExt;
use gstreamer::{
    self as gst,
    prelude::{ElementExt, GstBinExt},
};
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot};

use crate::GstRuntime;

#[derive(Clone, Copy, Debug)]
pub enum PeerRole {
    WhipPublisher,
    WhepSubscriber,
    MeetingParticipant,
}

pub type PeerId = String;

/// Outgoing ICE candidate event (server -> client)
#[derive(Clone, Debug)]
pub struct IceCandidate {
    pub mline_index: u32,
    pub candidate: String,
}

/// Optional callback sink for events coming from the peer.
/// Keep it `Send + Sync` so you can forward into tokio channels easily.
pub trait PeerEvents: Send + Sync + 'static {
    fn on_ice_candidate(&self, _peer_id: &PeerId, _cand: IceCandidate) {}
    fn on_state(&self, _peer_id: &PeerId, _state: PeerState) {}
}

#[derive(Clone, Copy, Debug)]
pub enum PeerState {
    New,
    Negotiating,
    Connected,
    Failed,
    Closed,
}

#[derive(Clone)]
pub struct PeerSession {
    gst: GstRuntime,
    inner: Arc<PeerInner>,
}

#[derive(Default)]
struct PeerHandlers {
    ice_handler: Option<glib::SignalHandlerId>,
    bus_watch: Option<BusWatchGuard>,
}

struct PeerInner {
    peer_id: PeerId,
    role: PeerRole,

    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,

    // Last produced local SDP (answer)
    local_sdp: Arc<RwLock<Option<String>>>,

    events: Option<Arc<dyn PeerEvents>>,

    // Pullable outgoing ICE for WHIP/WHEP HTTP layer (or WS signaling)
    ice_tx: broadcast::Sender<IceCandidate>,

    handlers: Arc<RwLock<PeerHandlers>>,
}

impl PeerSession {
    /// Create a new peer session with its own pipeline + webrtcbin.
    ///
    /// You must call `negotiate_as_answerer(offer_sdp)` to generate an answer.

    pub async fn new(
        gst_rt: GstRuntime,
        peer_id: PeerId,
        role: PeerRole,
        events: Option<Arc<dyn PeerEvents>>,
    ) -> Result<Self> {
        let peer_id_clone = peer_id.clone();
        let (ice_tx, _ice_rx) = broadcast::channel::<IceCandidate>(256);

        let (pipeline, webrtcbin) = gst_rt
            .exec(move || -> Result<(gst::Pipeline, gst::Element)> {
                let pipeline = gst::Pipeline::with_name(&format!("peer.pipeline.{peer_id_clone}"));

                let webrtcbin = gst::ElementFactory::make("webrtcbin")
                    .name("webrtc")
                    .build()
                    .map_err(|_| anyhow::anyhow!("Failed to create webrtcbin (plugin missing?)"))?;

                // IMPORTANT: don't ignore set_property result
                webrtcbin.set_property("bundle-policy", &"max-bundle");

                pipeline.add(&webrtcbin)?;
                Ok((pipeline, webrtcbin))
            })
            .await?;

        let session = Self {
            gst: gst_rt.clone(),
            inner: Arc::new(PeerInner {
                peer_id,
                role,
                pipeline,
                webrtcbin,
                local_sdp: Arc::new(RwLock::new(None)),
                events,
                ice_tx,
                handlers: Arc::new(RwLock::new(PeerHandlers::default())),
            }),
        };

        session.install_bus_watch().await?;
        session.install_event_handlers().await?;
        Ok(session)
    }

    pub fn id(&self) -> &PeerId {
        &self.inner.peer_id
    }

    pub fn role(&self) -> PeerRole {
        self.inner.role
    }

    /// Start the peer pipeline (PLAYING). You can do this before or after negotiate;
    /// most people do it immediately after creating the answer.
    pub async fn start(&self) -> Result<()> {
        let pipeline = self.inner.pipeline.clone();
        self.gst
            .exec(move || {
                pipeline.set_state(gst::State::Playing)?;
                Ok(())
            })
            .await
    }

    pub async fn stop(&self) -> Result<()> {
        let pipeline = self.inner.pipeline.clone();
        let webrtcbin = self.inner.webrtcbin.clone();
        let handlers = self.inner.handlers.clone();

        self.gst
            .exec(move || {
                let mut h = handlers.write();

                if let Some(id) = h.ice_handler.take() {
                    webrtcbin.disconnect(id);
                }

                if let Some(bus_watch) = h.bus_watch.take() {
                    drop(bus_watch);
                }

                pipeline.set_state(gst::State::Null)?;
                Ok(())
            })
            .await?;

        if let Some(ev) = &self.inner.events {
            ev.on_state(&self.inner.peer_id, PeerState::Closed);
        }

        Ok(())
    }

    /// Return the last produced local SDP (answer).
    pub fn local_sdp(&self) -> Option<String> {
        self.inner.local_sdp.read().clone()
    }

    /// Consumers can subscribe and "pull" outgoing ICE candidates.
    pub fn ice_subscribe(&self) -> broadcast::Receiver<IceCandidate> {
        self.inner.ice_tx.subscribe()
    }

    /// Trickle ICE candidate from client -> server.
    pub async fn add_ice_candidate(&self, mline_index: u32, candidate: String) -> Result<()> {
        let webrtcbin = self.inner.webrtcbin.clone();
        self.gst
            .exec(move || {
                webrtcbin.emit_by_name::<()>("add-ice-candidate", &[&mline_index, &candidate]);
                Ok(())
            })
            .await
    }

    /// Negotiate in "answerer" mode:
    /// - set remote offer
    /// - create answer
    /// - set local description
    ///
    /// Returns the SDP answer text.
    pub async fn negotiate_as_answerer(&self, offer_sdp: String) -> Result<String> {
        let webrtcbin = self.inner.webrtcbin.clone();
        let peer_id = self.inner.peer_id.clone();
        let local_sdp_store = self.inner.local_sdp.clone();
        let events = self.inner.events.clone();

        let (tx, rx) = oneshot::channel::<Result<String>>();

        self.gst
            .exec(move || -> Result<()> {
                if let Some(ev) = &events {
                    ev.on_state(&peer_id, PeerState::Negotiating);
                }

                let offer = parse_offer_sdp(&offer_sdp)?;

                webrtcbin
                    .emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

                let webrtcbin_for_promise = webrtcbin.clone();
                let peer_id_for_promise = peer_id.clone();
                let events_for_promise = events.clone();
                let local_sdp_store_for_promise = local_sdp_store.clone();

                let mut tx = Some(tx);

                let promise = gst::Promise::with_change_func(move |reply| {
                    let Some(tx) = tx.take() else {
                        return;
                    };

                    let Ok(Some(reply)) = reply else {
                        let _ = tx.send(Err(anyhow::anyhow!("create-answer promise got no reply")));
                        if let Some(ev) = &events_for_promise {
                            ev.on_state(&peer_id_for_promise, PeerState::Failed);
                        }
                        return;
                    };

                    let answer = match reply.get::<gst_webrtc::WebRTCSessionDescription>("answer") {
                        Ok(a) => a,
                        Err(e) => {
                            let _ = tx.send(Err(anyhow::anyhow!(
                                "create-answer reply missing/invalid 'answer': {e}"
                            )));
                            if let Some(ev) = &events_for_promise {
                                ev.on_state(&peer_id_for_promise, PeerState::Failed);
                            }
                            return;
                        }
                    };

                    webrtcbin_for_promise.emit_by_name::<()>(
                        "set-local-description",
                        &[&answer, &None::<gst::Promise>],
                    );

                    match sdp_to_string(answer.sdp()) {
                        Ok(txt) => {
                            *local_sdp_store_for_promise.write() = Some(txt.clone());
                            if let Some(ev) = &events_for_promise {
                                ev.on_state(&peer_id_for_promise, PeerState::Connected);
                            }
                            let _ = tx.send(Ok(txt));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            if let Some(ev) = &events_for_promise {
                                ev.on_state(&peer_id_for_promise, PeerState::Failed);
                            }
                        }
                    }
                });

                webrtcbin.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
                Ok(())
            })
            .await?;

        // Wait for answer (timeout so you don't hang forever)
        let res = tokio::time::timeout(std::time::Duration::from_secs(3), rx).await;

        match res {
            Ok(Ok(Ok(sdp))) => Ok(sdp),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_closed)) => Err(anyhow::anyhow!("answer channel closed")),
            Err(_timeout) => Err(anyhow::anyhow!("Timed out waiting for local SDP answer")),
        }
    }

    async fn install_event_handlers(&self) -> Result<()> {
        let webrtcbin = self.inner.webrtcbin.clone();
        let peer_id = self.inner.peer_id.clone();
        let events = self.inner.events.clone();
        let ice_tx = self.inner.ice_tx.clone();

        // Need mutable access to handler storage
        let handlers = self.inner.handlers.clone();

        self.gst
            .exec(move || -> Result<()> {
                // Store the handler id so we can disconnect later.
                let id = webrtcbin.connect("on-ice-candidate", false, move |values| {
                    let mlineindex = values[1].get::<u32>().unwrap_or(0);
                    let cand = values[2].get::<String>().unwrap_or_default();

                    let evt = IceCandidate {
                        mline_index: mlineindex,
                        candidate: cand.clone(),
                    };

                    // Push to pullable channel
                    let _ = ice_tx.send(evt.clone());

                    // Also send via optional callback
                    if let Some(ev) = &events {
                        ev.on_ice_candidate(&peer_id, evt);
                    } else {
                        tracing::debug!("[{peer_id}] ICE cand mline={mlineindex} cand={cand}");
                    }
                    None
                });

                handlers.write().ice_handler = Some(id);
                Ok(())
            })
            .await
    }

    async fn install_bus_watch(&self) -> Result<()> {
        let pipeline = self.inner.pipeline.clone();
        let peer_id = self.inner.peer_id.clone();
        let events = self.inner.events.clone();
        let handlers = self.inner.handlers.clone();

        self.gst
            .exec(move || -> Result<()> {
                let bus = pipeline
                    .bus()
                    .ok_or_else(|| anyhow::anyhow!("pipeline has no bus"))?;

                // add_watch_local requires we're on a GLib thread/context (we are).
                let guard = bus.add_watch_local(move |_, msg| {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::Error(e) => {
                            let src = e
                                .src()
                                .map(|s| s.path_string())
                                .unwrap_or_else(|| "<unknown>".into());
                            tracing::error!(
                                "[{peer_id}] GST ERROR from {src}: {} (debug: {:?})",
                                e.error(),
                                e.debug()
                            );
                            if let Some(ev) = &events {
                                ev.on_state(&peer_id, PeerState::Failed);
                            }
                            // Keep watching; you might want false if you prefer stopping logs after error.
                            ControlFlow::Continue
                        }
                        MessageView::Warning(w) => {
                            let src = w
                                .src()
                                .map(|s| s.path_string())
                                .unwrap_or_else(|| "<unknown>".into());
                            tracing::warn!(
                                "[{peer_id}] GST WARNING from {src}: {} (debug: {:?})",
                                w.error(),
                                w.debug()
                            );
                            ControlFlow::Continue
                        }
                        MessageView::StateChanged(s) => {
                            // Only log pipeline state changes (avoid huge spam)
                            if msg
                                .src()
                                .as_ref()
                                .is_some_and(|o| *o == pipeline.upcast_ref::<gst::Object>())
                            {
                                tracing::info!(
                                    "[{peer_id}] pipeline state: {:?} -> {:?} (pending {:?})",
                                    s.old(),
                                    s.current(),
                                    s.pending()
                                );
                            }
                            ControlFlow::Continue
                        }
                        _ => ControlFlow::Continue,
                    }
                })?;

                handlers.write().bus_watch = Some(guard);
                Ok(())
            })
            .await
    }
}

fn parse_offer_sdp(sdp_txt: &str) -> Result<gst_webrtc::WebRTCSessionDescription> {
    let msg = gst_sdp::SDPMessage::parse_buffer(sdp_txt.as_bytes())
        .map_err(|e| anyhow::anyhow!("SDP parse error: {e:?}"))?;

    Ok(gst_webrtc::WebRTCSessionDescription::new(
        gst_webrtc::WebRTCSDPType::Offer,
        msg,
    ))
}

fn sdp_to_string(msg: &gst_sdp::SDPMessageRef) -> Result<String> {
    Ok(msg.as_text()?.to_string())
}
