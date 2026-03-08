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
use glib::{ControlFlow, Priority, SourceId, object::ObjectExt};
use gstreamer::prelude::GstObjectExt;
use gstreamer::{
    self as gst,
    prelude::{ElementExt, ElementExtManual, GstBinExt, PadExt, PadExtManual},
};
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use parking_lot::RwLock;
use std::{env, sync::Arc};
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
    fn on_keyframe_request(&self, _peer_id: &PeerId) {}
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
    bus_watch: Option<SourceId>,
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
    whep_streams: Arc<RwLock<Vec<crate::RtpStreamInfo>>>,
    whep_injector: Arc<RwLock<Option<crate::RtpInjector>>>,
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

                webrtcbin.set_property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle);
                apply_webrtc_ice_config_from_env(&webrtcbin);

                match role {
                    PeerRole::WhipPublisher => {
                        add_transceiver(
                            &webrtcbin,
                            gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly,
                        );
                        add_transceiver(
                            &webrtcbin,
                            gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly,
                        );
                    }
                    PeerRole::WhepSubscriber => {}
                    PeerRole::MeetingParticipant => {
                        add_transceiver(
                            &webrtcbin,
                            gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                        );
                        add_transceiver(
                            &webrtcbin,
                            gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                        );
                    }
                }

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
                whep_streams: Arc::new(RwLock::new(Vec::new())),
                whep_injector: Arc::new(RwLock::new(None)),
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

    /// Move the peer pipeline to READY so negotiation can proceed.
    pub async fn start(&self) -> Result<()> {
        let pipeline = self.inner.pipeline.clone();
        self.gst
            .exec(move || {
                if let Err(err) = pipeline.set_state(gst::State::Ready) {
                    let (state_res, current, pending) =
                        pipeline.state(gst::ClockTime::from_mseconds(100));

                    let bus_detail = pipeline.bus().and_then(|bus| {
                        bus.pop_filtered(&[
                            gst::MessageType::Error,
                            gst::MessageType::Warning,
                            gst::MessageType::StateChanged,
                        ])
                        .map(|msg| match msg.view() {
                            gst::MessageView::Error(e) => format!(
                                "error from {}: {} debug={:?}",
                                e.src()
                                    .map(|s| s.path_string())
                                    .unwrap_or_else(|| "<unknown>".into()),
                                e.error(),
                                e.debug()
                            ),
                            gst::MessageView::Warning(w) => format!(
                                "warning from {}: {} debug={:?}",
                                w.src()
                                    .map(|s| s.path_string())
                                    .unwrap_or_else(|| "<unknown>".into()),
                                w.error(),
                                w.debug()
                            ),
                            gst::MessageView::StateChanged(s) => format!(
                                "state-changed {:?}->{:?} pending {:?}",
                                s.old(),
                                s.current(),
                                s.pending()
                            ),
                            _ => "other-message".to_string(),
                        })
                    });

                    return Err(anyhow::anyhow!(
                        "failed to set pipeline to READY: {err}; state_result={state_res:?}; current={current:?}; pending={pending:?}; bus={bus_detail:?}"
                    ));
                }
                Ok(())
            })
            .await
    }

    /// Move the peer pipeline to PLAYING once negotiation and wiring are complete.
    pub async fn play(&self) -> Result<()> {
        let pipeline = self.inner.pipeline.clone();
        self.gst
            .exec(move || {
                if let Err(err) = pipeline.set_state(gst::State::Playing) {
                    let (state_res, current, pending) =
                        pipeline.state(gst::ClockTime::from_mseconds(100));

                    let bus_detail = pipeline.bus().and_then(|bus| {
                        bus.pop_filtered(&[
                            gst::MessageType::Error,
                            gst::MessageType::Warning,
                            gst::MessageType::StateChanged,
                        ])
                        .map(|msg| match msg.view() {
                            gst::MessageView::Error(e) => format!(
                                "error from {}: {} debug={:?}",
                                e.src()
                                    .map(|s| s.path_string())
                                    .unwrap_or_else(|| "<unknown>".into()),
                                e.error(),
                                e.debug()
                            ),
                            gst::MessageView::Warning(w) => format!(
                                "warning from {}: {} debug={:?}",
                                w.src()
                                    .map(|s| s.path_string())
                                    .unwrap_or_else(|| "<unknown>".into()),
                                w.error(),
                                w.debug()
                            ),
                            gst::MessageView::StateChanged(s) => format!(
                                "state-changed {:?}->{:?} pending {:?}",
                                s.old(),
                                s.current(),
                                s.pending()
                            ),
                            _ => "other-message".to_string(),
                        })
                    });

                    return Err(anyhow::anyhow!(
                        "failed to set pipeline to PLAYING: {err}; state_result={state_res:?}; current={current:?}; pending={pending:?}; bus={bus_detail:?}"
                    ));
                }
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
                    bus_watch.remove();
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

    pub fn set_whep_streams(&self, streams: Vec<crate::RtpStreamInfo>) {
        *self.inner.whep_streams.write() = streams;
    }

    pub fn take_whep_injector(&self) -> Option<crate::RtpInjector> {
        self.inner.whep_injector.write().take()
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

    pub async fn request_keyframe(&self) -> Result<()> {
        let webrtcbin = self.inner.webrtcbin.clone();
        self.gst
            .exec(move || {
                let video_pad = find_video_src_pad(&webrtcbin)
                    .ok_or_else(|| anyhow::anyhow!("publisher video pad not available"))?;
                let event = gst::event::CustomUpstream::new(
                    gst::Structure::builder("GstForceKeyUnit")
                        .field("running-time", gst::ClockTime::NONE)
                        .field("all-headers", true)
                        .field("count", 0u32)
                        .build(),
                );

                if video_pad.send_event(event) {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "publisher video pad rejected upstream force-key-unit event"
                    ))
                }
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
        let pipeline = self.inner.pipeline.clone();
        let peer_id = self.inner.peer_id.clone();
        let role = self.inner.role;
        let local_sdp_store = self.inner.local_sdp.clone();
        let events = self.inner.events.clone();
        let whep_streams = self.inner.whep_streams.clone();
        let whep_injector = self.inner.whep_injector.clone();
        let keyframe_events = self.inner.events.clone();
        let keyframe_peer_id = self.inner.peer_id.clone();

        let (tx, rx) = oneshot::channel::<Result<String>>();

        self.gst
            .exec(move || -> Result<()> {
                if let Some(ev) = &events {
                    ev.on_state(&peer_id, PeerState::Negotiating);
                }

                let offer = parse_offer_sdp(&offer_sdp)?;
                let webrtcbin_for_remote = webrtcbin.clone();
                let pipeline_for_remote = pipeline.clone();
                let peer_id_for_remote = peer_id.clone();
                let role_for_remote = role;
                let events_for_remote = events.clone();
                let local_sdp_store_for_remote = local_sdp_store.clone();
                let whep_streams_for_remote = whep_streams.clone();
                let whep_injector_for_remote = whep_injector.clone();

                let mut tx = Some(tx);

                let set_remote_promise = gst::Promise::with_change_func(move |reply| {
                    let Some(tx) = tx.take() else {
                        return;
                    };

                    match reply {
                        Ok(Some(_)) | Ok(None) => {}
                        Err(err) => {
                            let _ = tx.send(Err(anyhow::anyhow!(
                                "set-remote-description promise failed: {err:?}"
                            )));
                            if let Some(ev) = &events_for_remote {
                                ev.on_state(&peer_id_for_remote, PeerState::Failed);
                            }
                            return;
                        }
                    }

                    if matches!(role_for_remote, PeerRole::WhepSubscriber) {
                        set_all_transceivers_direction(
                            &webrtcbin_for_remote,
                            gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
                        );
                        let streams = whep_streams_for_remote.read().clone();
                        if !streams.is_empty() {
                            let on_video_keyframe_request = keyframe_events.clone().map(|events| {
                                let peer_id = keyframe_peer_id.clone();
                                Arc::new(move || {
                                    tracing::info!(
                                        "[{peer_id}] subscriber requested a video keyframe"
                                    );
                                    events.on_keyframe_request(&peer_id);
                                }) as Arc<dyn Fn() + Send + Sync>
                            });
                            match crate::install_rtp_injector(
                                &pipeline_for_remote,
                                &webrtcbin_for_remote,
                                streams,
                                on_video_keyframe_request,
                            ) {
                                Ok(injector) => {
                                    *whep_injector_for_remote.write() = Some(injector);
                                }
                                Err(err) => {
                                    let _ = tx.send(Err(anyhow::anyhow!(
                                        "failed to install whep injector during negotiation: {err}"
                                    )));
                                    if let Some(ev) = &events_for_remote {
                                        ev.on_state(&peer_id_for_remote, PeerState::Failed);
                                    }
                                    return;
                                }
                            }
                        }
                    } else if matches!(role_for_remote, PeerRole::MeetingParticipant) {
                        // Meeting peers both send local media and receive subscribed remote media.
                        // Keep directions explicit so answer SDP doesn't collapse to recvonly.
                        set_all_transceivers_direction(
                            &webrtcbin_for_remote,
                            gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                        );
                    }

                    let webrtcbin_for_answer = webrtcbin_for_remote.clone();
                    let peer_id_for_answer = peer_id_for_remote.clone();
                    let role_for_answer = role_for_remote;
                    let events_for_answer = events_for_remote.clone();
                    let local_sdp_store_for_answer = local_sdp_store_for_remote.clone();

                    let mut tx = Some(tx);

                    let create_answer_promise = gst::Promise::with_change_func(move |reply| {
                        let Some(tx) = tx.take() else {
                            return;
                        };

                        let Ok(Some(reply)) = reply else {
                            let _ = tx.send(Err(anyhow::anyhow!(
                                "create-answer promise got no reply"
                            )));
                            if let Some(ev) = &events_for_answer {
                                ev.on_state(&peer_id_for_answer, PeerState::Failed);
                            }
                            return;
                        };

                        let reply_debug = format!("{reply:?}");
                        if let Ok(err) = reply.get::<glib::Error>("error") {
                            let _ = tx.send(Err(anyhow::anyhow!(
                                "create-answer failed: {err}; reply={reply_debug}"
                            )));
                            if let Some(ev) = &events_for_answer {
                                ev.on_state(&peer_id_for_answer, PeerState::Failed);
                            }
                            return;
                        }

                        let answer = match reply.get::<gst_webrtc::WebRTCSessionDescription>("answer")
                        {
                            Ok(a) => a,
                            Err(e) => {
                                let _ = tx.send(Err(anyhow::anyhow!(
                                    "create-answer reply missing/invalid 'answer': {e}; reply={reply_debug}"
                                )));
                                if let Some(ev) = &events_for_answer {
                                    ev.on_state(&peer_id_for_answer, PeerState::Failed);
                                }
                                return;
                            }
                        };

                        webrtcbin_for_answer.emit_by_name::<()>(
                            "set-local-description",
                            &[&answer, &None::<gst::Promise>],
                        );

                        match sdp_to_string(answer.sdp()) {
                            Ok(txt) => {
                                let txt = if matches!(role_for_answer, PeerRole::MeetingParticipant)
                                {
                                    normalize_answer_setup_attr(txt)
                                } else {
                                    txt
                                };
                                if matches!(role_for_answer, PeerRole::WhepSubscriber) {
                                    let transceivers = describe_transceivers(&webrtcbin_for_answer);
                                    let inactive = inactive_media_sections(&txt);
                                    if !inactive.is_empty() {
                                        let _ = tx.send(Err(anyhow::anyhow!(
                                            "whep answer negotiated inactive media sections {:?}; transceivers={:?}; sdp={}",
                                            inactive,
                                            transceivers,
                                            txt
                                        )));
                                        if let Some(ev) = &events_for_answer {
                                            ev.on_state(&peer_id_for_answer, PeerState::Failed);
                                        }
                                        return;
                                    }
                                }
                                *local_sdp_store_for_answer.write() = Some(txt.clone());
                                if let Some(ev) = &events_for_answer {
                                    ev.on_state(&peer_id_for_answer, PeerState::Connected);
                                }
                                let _ = tx.send(Ok(txt));
                            }
                            Err(e) => {
                                let _ = tx.send(Err(e));
                                if let Some(ev) = &events_for_answer {
                                    ev.on_state(&peer_id_for_answer, PeerState::Failed);
                                }
                            }
                        }
                    });

                    webrtcbin_for_remote.emit_by_name::<()>(
                        "create-answer",
                        &[&None::<gst::Structure>, &create_answer_promise],
                    );
                });

                webrtcbin.emit_by_name::<()>(
                    "set-remote-description",
                    &[&offer, &set_remote_promise],
                );
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
                let watch = bus.create_watch(None, Priority::DEFAULT, move |_, msg| {
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
                });

                handlers.write().bus_watch = Some(watch.attach(None));
                Ok(())
            })
            .await
    }
    pub async fn install_rtp_tap(&self) -> Result<crate::RtpTap> {
        let pipeline = self.inner.pipeline.clone();
        let webrtcbin = self.inner.webrtcbin.clone();
        self.gst
            .exec(move || crate::install_rtp_tap(&pipeline, &webrtcbin))
            .await
    }

    pub async fn install_rtp_injector(
        &self,
        initial_streams: Vec<crate::RtpStreamInfo>,
    ) -> Result<crate::RtpInjector> {
        let pipeline = self.inner.pipeline.clone();
        let webrtcbin = self.inner.webrtcbin.clone();
        self.gst
            .exec(move || {
                crate::install_rtp_injector(&pipeline, &webrtcbin, initial_streams, None)
            })
            .await
    }
}

fn normalize_answer_setup_attr(sdp: String) -> String {
    // In renegotiation some stacks may emit actpass (or duplicate setup lines)
    // in generated answer SDP. Browsers require answer setup to be active/passive
    // and tolerate only one setup line per media section.
    let normalized = sdp
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| line.to_string())
        .collect::<Vec<_>>();

    let mut out = Vec::with_capacity(normalized.len());
    let mut in_media = false;
    let mut saw_setup_in_media = false;

    for mut line in normalized {
        if line.is_empty() {
            continue;
        }

        if line.starts_with("m=") {
            in_media = true;
            saw_setup_in_media = false;
            out.push(line);
            continue;
        }

        if line.starts_with("a=setup:") {
            if in_media {
                if saw_setup_in_media {
                    continue;
                }
                if line == "a=setup:actpass" || line == "a=setup:holdconn" {
                    line = "a=setup:active".to_string();
                }
                saw_setup_in_media = true;
            } else if line == "a=setup:actpass" || line == "a=setup:holdconn" {
                line = "a=setup:active".to_string();
            }
            out.push(line);
            continue;
        }

        out.push(line);
    }

    let mut joined = out.join("\r\n");
    joined.push_str("\r\n");
    joined
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

fn add_transceiver(webrtcbin: &gst::Element, direction: gst_webrtc::WebRTCRTPTransceiverDirection) {
    let _ = webrtcbin.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
        "add-transceiver",
        &[&direction, &None::<gst::Caps>],
    );
}

fn describe_transceivers(webrtcbin: &gst::Element) -> Vec<String> {
    let mut out = Vec::new();
    for idx in 0..8i32 {
        let transceiver = webrtcbin
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&idx]);
        let Some(transceiver) = transceiver else {
            break;
        };
        out.push(format!(
            "idx={idx} mline={} dir={:?} sender={} receiver={}",
            transceiver.mlineindex(),
            transceiver
                .property_value("direction")
                .get::<gst_webrtc::WebRTCRTPTransceiverDirection>()
                .ok(),
            transceiver.sender().is_some(),
            transceiver.receiver().is_some(),
        ));
    }
    out
}

fn set_all_transceivers_direction(
    webrtcbin: &gst::Element,
    direction: gst_webrtc::WebRTCRTPTransceiverDirection,
) {
    for idx in 0..8i32 {
        let transceiver = webrtcbin
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&idx]);
        let Some(transceiver) = transceiver else {
            break;
        };
        transceiver.set_property("direction", direction);
    }
}

fn inactive_media_sections(sdp: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current_m = None::<String>;
    let mut current_mid = None::<String>;

    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=") {
            current_m = Some(rest.to_string());
            current_mid = None;
        } else if let Some(rest) = line.strip_prefix("a=mid:") {
            current_mid = Some(rest.to_string());
        } else if line == "a=inactive" {
            out.push(
                current_mid
                    .clone()
                    .or_else(|| current_m.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
            );
        }
    }

    out
}

fn find_video_src_pad(webrtcbin: &gst::Element) -> Option<gst::Pad> {
    webrtcbin.src_pads().into_iter().find(|pad| {
        pad.current_caps()
            .as_ref()
            .is_some_and(is_video_rtp_caps)
            || pad
                .allowed_caps()
                .as_ref()
                .is_some_and(is_video_rtp_caps)
    })
}

fn is_video_rtp_caps(caps: &gst::Caps) -> bool {
    caps.structure(0).is_some_and(|s| {
        s.name() == "application/x-rtp"
            && s.get_optional::<String>("media")
                .ok()
                .flatten()
                .as_deref()
                == Some("video")
    })
}

fn apply_webrtc_ice_config_from_env(webrtcbin: &gst::Element) {
    // GStreamer expects:
    //   stun-server = "stun://host:port"
    //   turn-server = "turn://user:pass@host:port?transport=udp"
    if let Ok(stun) = env::var("NARWHAL_STUN_SERVER") {
        if !stun.trim().is_empty() {
            webrtcbin.set_property("stun-server", stun.trim());
        }
    }
    if let Ok(turn) = env::var("NARWHAL_TURN_SERVER") {
        if !turn.trim().is_empty() {
            webrtcbin.set_property("turn-server", turn.trim());
        }
    }
}
