mod errors;
mod ws;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use self::errors::TransportError;
use narwhal_core::{BroadcastPolicyMode, RoomId, RoomManager};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeetingNegotiationState {
    AwaitingInitialOffer,
    Stable,
    RenegotiationRequired,
}

impl MeetingNegotiationState {
    pub fn as_label(self) -> &'static str {
        match self {
            MeetingNegotiationState::AwaitingInitialOffer => "awaiting_initial_offer",
            MeetingNegotiationState::Stable => "stable",
            MeetingNegotiationState::RenegotiationRequired => "renegotiation_required",
        }
    }
}

#[derive(Clone, Debug)]
pub struct MeetingRenegotiationNotification {
    pub revision: u64,
    pub reason: &'static str,
    pub negotiation_state: MeetingNegotiationState,
}

#[derive(Clone, Default)]
pub struct MeetingSessionRegistry {
    inner: Arc<Mutex<HashMap<String, HashMap<String, RegisteredMeetingSession>>>>,
}

#[derive(Clone)]
struct RegisteredMeetingSession {
    tx: tokio::sync::mpsc::UnboundedSender<MeetingRenegotiationNotification>,
    negotiation_state: MeetingNegotiationState,
}

impl MeetingSessionRegistry {
    pub fn register(
        &self,
        room: &RoomId,
        participant_id: &str,
        tx: tokio::sync::mpsc::UnboundedSender<MeetingRenegotiationNotification>,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .entry(room.0.clone())
                .or_default()
                .insert(
                    participant_id.to_string(),
                    RegisteredMeetingSession {
                        tx,
                        negotiation_state: MeetingNegotiationState::AwaitingInitialOffer,
                    },
                );
        }
    }

    pub fn unregister(&self, room: &RoomId, participant_id: &str) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(room_sessions) = inner.get_mut(&room.0)
        {
            room_sessions.remove(participant_id);
            if room_sessions.is_empty() {
                inner.remove(&room.0);
            }
        }
    }

    pub fn set_negotiation_state(
        &self,
        room: &RoomId,
        participant_id: &str,
        negotiation_state: MeetingNegotiationState,
    ) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(room_sessions) = inner.get_mut(&room.0)
            && let Some(session) = room_sessions.get_mut(participant_id)
        {
            session.negotiation_state = negotiation_state;
        }
    }

    pub fn notify_participants(
        &self,
        room: &RoomId,
        participant_ids: &[String],
        revision: u64,
        reason: &'static str,
    ) {
        let mut stale = Vec::new();
        if let Ok(mut inner) = self.inner.lock()
            && let Some(room_sessions) = inner.get_mut(&room.0)
        {
            for participant_id in participant_ids {
                let Some(session) = room_sessions.get_mut(participant_id) else {
                    continue;
                };
                if matches!(
                    session.negotiation_state,
                    MeetingNegotiationState::AwaitingInitialOffer
                ) {
                    continue;
                }

                session.negotiation_state = MeetingNegotiationState::RenegotiationRequired;
                if session
                    .tx
                    .send(MeetingRenegotiationNotification {
                        revision,
                        reason,
                        negotiation_state: MeetingNegotiationState::RenegotiationRequired,
                    })
                    .is_err()
                {
                    stale.push(participant_id.clone());
                }
            }

            for participant_id in stale {
                room_sessions.remove(&participant_id);
            }
            if room_sessions.is_empty() {
                inner.remove(&room.0);
            }
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    rooms: RoomManager,
    meeting_sessions: MeetingSessionRegistry,
    ready: Arc<AtomicBool>,
    readiness_cache: Arc<Mutex<Option<ReadinessCacheEntry>>>,
    readiness_cache_ttl: Duration,
}

#[derive(Clone, Copy)]
struct ReadinessCacheEntry {
    checked_at: Instant,
    ready: bool,
}

impl AppState {
    pub fn new(rooms: RoomManager) -> Self {
        Self::with_readiness_cache_ttl(rooms, Duration::from_secs(1))
    }

    pub fn with_readiness_cache_ttl(rooms: RoomManager, readiness_cache_ttl: Duration) -> Self {
        Self {
            rooms,
            meeting_sessions: MeetingSessionRegistry::default(),
            ready: Arc::new(AtomicBool::new(true)),
            readiness_cache: Arc::new(Mutex::new(None)),
            readiness_cache_ttl,
        }
    }

    pub fn rooms(&self) -> RoomManager {
        self.rooms.clone()
    }

    pub fn meeting_sessions(&self) -> MeetingSessionRegistry {
        self.meeting_sessions.clone()
    }

    pub fn mark_draining(&self) {
        self.ready.store(false, Ordering::SeqCst);
        if let Ok(mut cache) = self.readiness_cache.lock() {
            *cache = Some(ReadinessCacheEntry {
                checked_at: Instant::now(),
                ready: false,
            });
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub async fn readiness(&self) -> bool {
        if !self.is_ready() {
            return false;
        }

        if let Ok(cache) = self.readiness_cache.lock()
            && let Some(entry) = *cache
            && entry.checked_at.elapsed() < self.readiness_cache_ttl
        {
            return entry.ready;
        }

        let ready = self.rooms.probe_media_ready().await.is_ok();
        if let Ok(mut cache) = self.readiness_cache.lock() {
            *cache = Some(ReadinessCacheEntry {
                checked_at: Instant::now(),
                ready,
            });
        }
        ready
    }

    pub fn draining_response(&self) -> axum::response::Response {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "server is draining and not accepting new sessions"
            })),
        )
            .into_response()
    }
}

pub fn app(rooms: RoomManager) -> Router {
    app_with_state(AppState::new(rooms))
}

pub fn app_with_state(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(get_health))
        .route("/readyz", get(get_ready))
        .route("/whip/{room}", post(whip_post))
        .route("/whip/{room}/{pub}/ice", get(whip_get_ice))
        .route("/whip/{room}/{pub}", delete(whip_delete).patch(whip_patch))
        .route("/whep/{room}", post(whep_post))
        .route("/whep/{room}/{sub}/ice", get(whep_get_ice))
        .route("/whep/{room}/{sub}", delete(whep_delete).patch(whep_patch))
        .route(
            "/control/rooms/{room}/broadcast/policy",
            get(get_broadcast_policy_mode).put(set_broadcast_policy_mode),
        )
        .route(
            "/control/rooms/{room}/broadcast",
            get(get_broadcast_inspection),
        )
        .route("/metrics", get(get_metrics))
        .route("/ws", get(ws::ws_upgrade))
        .with_state(state)
}

async fn get_health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

async fn get_ready(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({ "ready": state.readiness().await }))
}

#[derive(Deserialize)]
struct IceIn {
    mline_index: u32,
    candidate: String,
}

#[derive(Serialize)]
struct IceOut {
    mline_index: u32,
    candidate: String,
}

#[derive(Serialize)]
struct BroadcastPolicyModeOut {
    mode: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireBroadcastPolicyMode {
    Standard,
    LowBandwidth,
    AudioOnly,
}

impl From<WireBroadcastPolicyMode> for BroadcastPolicyMode {
    fn from(value: WireBroadcastPolicyMode) -> Self {
        match value {
            WireBroadcastPolicyMode::Standard => BroadcastPolicyMode::Standard,
            WireBroadcastPolicyMode::LowBandwidth => BroadcastPolicyMode::LowBandwidth,
            WireBroadcastPolicyMode::AudioOnly => BroadcastPolicyMode::AudioOnly,
        }
    }
}

#[derive(Deserialize)]
struct SetBroadcastPolicyModeIn {
    mode: WireBroadcastPolicyMode,
}

fn broadcast_policy_mode_label(mode: BroadcastPolicyMode) -> &'static str {
    match mode {
        BroadcastPolicyMode::Standard => "standard",
        BroadcastPolicyMode::LowBandwidth => "low_bandwidth",
        BroadcastPolicyMode::AudioOnly => "audio_only",
    }
}

async fn whip_post(
    State(state): State<AppState>,
    Path(room): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    if !state.is_ready() {
        return state.draining_response();
    }

    let offer_sdp = match std::str::from_utf8(&body) {
        Ok(s) => s.to_string(),
        Err(_) => return TransportError::invalid_request("invalid utf8 SDP").into_response(),
    };

    match state.rooms().whip_publish(RoomId(room), offer_sdp).await {
        Ok(res) => {
            let mut h = HeaderMap::new();
            h.insert("content-type", "application/sdp".parse().unwrap());
            h.insert("location", res.location.parse().unwrap());
            (StatusCode::CREATED, h, res.answer_sdp).into_response()
        }
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn whep_post(
    State(state): State<AppState>,
    Path(room): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    if !state.is_ready() {
        return state.draining_response();
    }

    let offer_sdp = match std::str::from_utf8(&body) {
        Ok(s) => s.to_string(),
        Err(_) => return TransportError::invalid_request("invalid utf8 SDP").into_response(),
    };

    match state.rooms().whep_subscribe(RoomId(room), offer_sdp).await {
        Ok(res) => {
            let mut h = HeaderMap::new();
            h.insert("content-type", "application/sdp".parse().unwrap());
            h.insert("location", res.location.parse().unwrap());
            (StatusCode::CREATED, h, res.answer_sdp).into_response()
        }
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn whip_patch(
    State(state): State<AppState>,
    Path((room, pub_id)): Path<(String, String)>,
    Json(payload): Json<IceIn>,
) -> impl IntoResponse {
    match state
        .rooms()
        .whip_trickle(
            RoomId(room),
            &pub_id,
            payload.mline_index,
            payload.candidate,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn whep_patch(
    State(state): State<AppState>,
    Path((room, sub_id)): Path<(String, String)>,
    Json(payload): Json<IceIn>,
) -> impl IntoResponse {
    match state
        .rooms()
        .whep_trickle(
            RoomId(room),
            &sub_id,
            payload.mline_index,
            payload.candidate,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn whip_get_ice(
    State(state): State<AppState>,
    Path((room, pub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.rooms().whip_drain_ice(RoomId(room), &pub_id, 50) {
        Ok(list) => {
            let out: Vec<IceOut> = list
                .into_iter()
                .map(|c| IceOut {
                    mline_index: c.mline_index,
                    candidate: c.candidate,
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn whep_get_ice(
    State(state): State<AppState>,
    Path((room, sub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.rooms().whep_drain_ice(RoomId(room), &sub_id, 50) {
        Ok(list) => {
            let out: Vec<IceOut> = list
                .into_iter()
                .map(|c| IceOut {
                    mline_index: c.mline_index,
                    candidate: c.candidate,
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn whip_delete(
    State(state): State<AppState>,
    Path((room, pub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.rooms().whip_stop(RoomId(room), &pub_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn whep_delete(
    State(state): State<AppState>,
    Path((room, sub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.rooms().whep_stop(RoomId(room), &sub_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn get_broadcast_policy_mode(
    State(state): State<AppState>,
    Path(room): Path<String>,
) -> impl IntoResponse {
    match state.rooms().broadcast_policy_mode(RoomId(room)) {
        Ok(mode) => Json(BroadcastPolicyModeOut {
            mode: broadcast_policy_mode_label(mode),
        })
        .into_response(),
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn set_broadcast_policy_mode(
    State(state): State<AppState>,
    Path(room): Path<String>,
    Json(payload): Json<SetBroadcastPolicyModeIn>,
) -> impl IntoResponse {
    let mode: BroadcastPolicyMode = payload.mode.into();
    match state.rooms().broadcast_set_policy_mode(RoomId(room), mode) {
        Ok(()) => Json(BroadcastPolicyModeOut {
            mode: broadcast_policy_mode_label(mode),
        })
        .into_response(),
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn get_broadcast_inspection(
    State(state): State<AppState>,
    Path(room): Path<String>,
) -> impl IntoResponse {
    match state.rooms().broadcast_inspect(RoomId(room)) {
        Ok(info) => Json(serde_json::json!({
            "mode": broadcast_policy_mode_label(info.policy_mode),
            "publisher_id": info.publisher_id,
            "subscriber_ids": info.subscriber_ids,
            "stream_infos": info.stream_infos.into_iter().map(|stream| serde_json::json!({
                "media_key": stream.media_key,
                "caps": stream.caps,
            })).collect::<Vec<_>>(),
            "subscriber_plans": info.subscriber_plans.into_iter().map(|plan| serde_json::json!({
                "subscriber_id": plan.subscriber_id,
                "audio_track_ids": plan.audio_track_ids,
                "video": plan.video.into_iter().map(|selection| serde_json::json!({
                    "track_id": selection.track_id,
                    "target": match selection.target {
                        narwhal_core::VideoTarget::High => "high",
                        narwhal_core::VideoTarget::Medium => "medium",
                        narwhal_core::VideoTarget::Low => "low",
                    },
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "graph_edges": info.graph_edges.into_iter().map(|edge| serde_json::json!({
                "track_id": edge.track_id,
                "subscriber_id": edge.subscriber_id,
                "kind": edge.kind,
                "video_target": edge.video_target.map(|target| match target {
                    narwhal_core::VideoTarget::High => "high",
                    narwhal_core::VideoTarget::Medium => "medium",
                    narwhal_core::VideoTarget::Low => "low",
                }),
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => TransportError::from_anyhow(e).into_response(),
    }
}

async fn get_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    (StatusCode::OK, headers, state.rooms().render_metrics())
}

#[cfg(test)]
mod tests {
    use super::{AppState, app, app_with_state, get_metrics};
    use axum::{
        body::{Body, to_bytes},
        extract::State,
        http::{Request, StatusCode, header},
        response::IntoResponse,
    };
    use futures_util::{SinkExt, StreamExt};
    use media::GstRuntime;
    use narwhal_core::{MediaKind, MeetingPublishTrack, RoomId, RoomManager, RoomMode};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
    use tower::util::ServiceExt;

    fn manager() -> RoomManager {
        let gst = GstRuntime::init().expect("gstreamer runtime must initialize");
        RoomManager::new(gst)
    }

    fn browser_like_offer_sdp() -> String {
        "\
v=0\r\n\
o=- 4611733055804614137 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
a=msid-semantic: WMS\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:someufrag\r\n\
a=ice-pwd:somepassword1234567890\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n\
a=ssrc:1234 cname:test\r\n\
a=ssrc:1234 msid:test audio0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:someufrag\r\n\
a=ice-pwd:somepassword1234567890\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=ssrc:5678 cname:test\r\n\
a=ssrc:5678 msid:test video0\r\n"
            .to_string()
    }

    fn browser_like_subscribe_offer_sdp() -> String {
        "\
v=0\r\n\
o=- 4611733055804614138 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:subufrag\r\n\
a=ice-pwd:subpassword1234567890\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:subufrag\r\n\
a=ice-pwd:subpassword1234567890\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 VP8/90000\r\n"
            .to_string()
    }

    async fn spawn_ws_app(
        rooms: RoomManager,
    ) -> std::io::Result<(String, tokio::task::JoinHandle<std::io::Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app(rooms))
                .await
                .map_err(std::io::Error::other)
        });
        Ok((format!("ws://{addr}/ws"), handle))
    }

    async fn ws_connect(url: &str) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
        let (stream, _) = connect_async(url).await.expect("ws connect succeeds");
        stream
    }

    async fn ws_rpc(
        ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        id: u64,
        method: &str,
        params: Value,
    ) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        ws.send(Message::Text(request.to_string().into()))
            .await
            .expect("ws send succeeds");

        let message = ws.next().await.expect("response frame").expect("ws frame");
        let Message::Text(text) = message else {
            panic!("expected text frame");
        };
        let response: Value = serde_json::from_str(&text).expect("json response");
        if let Some(error) = response.get("error") {
            panic!("rpc {method} failed: {error}");
        }
        response["result"].clone()
    }

    async fn ws_next_json(
        ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    ) -> Value {
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .expect("websocket frame should arrive before timeout")
            .expect("response frame")
            .expect("ws frame");
        let Message::Text(text) = message else {
            panic!("expected text frame");
        };
        serde_json::from_str(&text).expect("json frame")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metrics_endpoint_returns_prometheus_payload() {
        let response = get_metrics(State(AppState::new(manager())))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; version=0.0.4; charset=utf-8"
        );

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("metrics body readable");
        let body = String::from_utf8(body.to_vec()).expect("metrics body utf-8");

        assert!(body.contains("narwhal_rooms_total"));
        assert!(body.contains("narwhal_meeting_joins_total"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn health_endpoint_returns_ok() {
        let response = app(manager())
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .method("GET")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json, json!({ "ok": true }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn readiness_endpoint_returns_ready() {
        let response = app(manager())
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .method("GET")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json, json!({ "ready": true }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn readiness_endpoint_returns_false_when_draining() {
        let state = AppState::new(manager());
        state.mark_draining();

        let response = app_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .method("GET")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json, json!({ "ready": false }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whip_post_returns_service_unavailable_when_draining() {
        let state = AppState::new(manager());
        state.mark_draining();

        let response = app_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/whip/draining-room")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json,
            json!({ "error": "server is draining and not accepting new sessions" })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whep_post_returns_service_unavailable_when_draining() {
        let state = AppState::new(manager());
        state.mark_draining();

        let response = app_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/whep/draining-room")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_subscribe_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json,
            json!({ "error": "server is draining and not accepting new sessions" })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_policy_endpoint_returns_not_found_for_missing_room() {
        let response = app(manager())
            .oneshot(
                Request::builder()
                    .uri("/control/rooms/missing-room/broadcast/policy")
                    .method("GET")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["error"], "room not found");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_policy_endpoint_returns_conflict_for_meeting_room() {
        let rooms = manager();
        rooms.ensure_room_mode(RoomId("meeting-room".to_string()), RoomMode::Meeting);

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/control/rooms/meeting-room/broadcast/policy")
                    .method("GET")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json["error"],
            "room is in meeting mode; WHIP/WHEP broadcast endpoints are not available"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_broadcast_policy_endpoint_updates_mode() {
        let rooms = manager();
        rooms.ensure_room_mode(RoomId("broadcast-room".to_string()), RoomMode::Broadcast);

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/control/rooms/broadcast-room/broadcast/policy")
                    .method("PUT")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"mode":"audio_only"}"#))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["mode"], "audio_only");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whep_post_returns_not_found_when_room_is_missing() {
        let response = app(manager())
            .oneshot(
                Request::builder()
                    .uri("/whep/missing-room")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from("dummy-offer"))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["error"], "room not found");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whep_post_returns_not_found_when_no_publisher_exists() {
        let rooms = manager();
        rooms.ensure_room_mode(
            RoomId("broadcast-no-publisher".to_string()),
            RoomMode::Broadcast,
        );

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/whep/broadcast-no-publisher")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from("dummy-offer"))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["error"], "no publisher");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whip_post_accepts_browser_like_sdp_offer() {
        let response = app(manager())
            .oneshot(
                Request::builder()
                    .uri("/whip/broadcast-whip-ok")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/sdp"
        );
        let location = response
            .headers()
            .get(header::LOCATION)
            .expect("location header present")
            .to_str()
            .expect("location header utf-8");
        assert!(location.starts_with("/whip/broadcast-whip-ok/"));

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let answer = String::from_utf8(body.to_vec()).expect("answer utf-8");
        assert!(answer.contains("m=audio"));
        assert!(answer.contains("m=video"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whip_delete_is_idempotent_for_already_stopped_publisher() {
        let rooms = manager();
        let publish_response = app(rooms.clone())
            .oneshot(
                Request::builder()
                    .uri("/whip/broadcast-whip-delete")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");
        let location = publish_response
            .headers()
            .get(header::LOCATION)
            .expect("location header present")
            .to_str()
            .expect("location header utf-8")
            .to_string();

        let first_delete = app(rooms.clone())
            .oneshot(
                Request::builder()
                    .uri(&location)
                    .method("DELETE")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("first delete response available");
        assert_eq!(first_delete.status(), StatusCode::NO_CONTENT);

        let second_delete = app(rooms)
            .oneshot(
                Request::builder()
                    .uri(&location)
                    .method("DELETE")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("second delete response available");
        assert_eq!(second_delete.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whep_post_returns_unprocessable_with_real_sdp_when_publisher_not_flowing() {
        let rooms = manager();
        let publisher_response = app(rooms.clone())
            .oneshot(
                Request::builder()
                    .uri("/whip/broadcast-not-flowing")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");
        assert_eq!(publisher_response.status(), StatusCode::CREATED);

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/whep/broadcast-not-flowing")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_subscribe_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json["error"],
            "publisher has not produced RTP yet; try subscribing after media is flowing"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whep_post_accepts_browser_like_sdp_when_streams_are_seeded() {
        let rooms = manager();
        let publisher_response = app(rooms.clone())
            .oneshot(
                Request::builder()
                    .uri("/whip/broadcast-whep-ok")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");
        assert_eq!(publisher_response.status(), StatusCode::CREATED);

        rooms
            .seed_broadcast_streams_for_test(RoomId("broadcast-whep-ok".to_string()))
            .expect("seeded broadcast streams");

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/whep/broadcast-whep-ok")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_subscribe_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/sdp"
        );
        let location = response
            .headers()
            .get(header::LOCATION)
            .expect("location header present")
            .to_str()
            .expect("location header utf-8");
        assert!(location.starts_with("/whep/broadcast-whep-ok/"));

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let answer = String::from_utf8(body.to_vec()).expect("answer utf-8");
        assert!(answer.contains("m=audio"));
        assert!(answer.contains("m=video"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whep_delete_is_idempotent_for_already_stopped_subscriber() {
        let rooms = manager();
        let publisher_response = app(rooms.clone())
            .oneshot(
                Request::builder()
                    .uri("/whip/broadcast-whep-delete")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");
        assert_eq!(publisher_response.status(), StatusCode::CREATED);

        rooms
            .seed_broadcast_streams_for_test(RoomId("broadcast-whep-delete".to_string()))
            .expect("seeded broadcast streams");

        let subscribe_response = app(rooms.clone())
            .oneshot(
                Request::builder()
                    .uri("/whep/broadcast-whep-delete")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(browser_like_subscribe_offer_sdp()))
                    .expect("request builds"),
            )
            .await
            .expect("response available");
        assert_eq!(subscribe_response.status(), StatusCode::CREATED);

        let location = subscribe_response
            .headers()
            .get(header::LOCATION)
            .expect("location header present")
            .to_str()
            .expect("location header utf-8")
            .to_string();

        let first_delete = app(rooms.clone())
            .oneshot(
                Request::builder()
                    .uri(&location)
                    .method("DELETE")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("first delete response available");
        assert_eq!(first_delete.status(), StatusCode::NO_CONTENT);

        let second_delete = app(rooms)
            .oneshot(
                Request::builder()
                    .uri(&location)
                    .method("DELETE")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("second delete response available");
        assert_eq!(second_delete.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whep_post_returns_conflict_for_meeting_room() {
        let rooms = manager();
        rooms.ensure_room_mode(RoomId("meeting-whep".to_string()), RoomMode::Meeting);

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/whep/meeting-whep")
                    .method("POST")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from("dummy-offer"))
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json["error"],
            "room is in meeting mode; WHIP/WHEP broadcast endpoints are not available"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whip_ice_endpoint_returns_conflict_for_meeting_room() {
        let rooms = manager();
        rooms.ensure_room_mode(RoomId("meeting-whip-ice".to_string()), RoomMode::Meeting);

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/whip/meeting-whip-ice/publisher-1/ice")
                    .method("GET")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json["error"],
            "room is in meeting mode; WHIP/WHEP broadcast endpoints are not available"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metrics_endpoint_reflects_room_activity_and_failure_labels() {
        let rooms = manager();

        rooms
            .meeting_join(
                RoomId("meeting-a".to_string()),
                "alice".to_string(),
                Some("Alice".to_string()),
            )
            .expect("alice joins");
        rooms
            .meeting_publish_tracks(
                RoomId("meeting-a".to_string()),
                "alice",
                vec![MeetingPublishTrack {
                    track_id: "alice-audio".to_string(),
                    media_kind: MediaKind::Audio,
                    mid: Some("0".to_string()),
                }],
            )
            .expect("alice publishes");

        rooms.ensure_room_mode(RoomId("broadcast-a".to_string()), RoomMode::Broadcast);
        let _ = rooms
            .whep_subscribe(RoomId("broadcast-a".to_string()), "dummy-offer".to_string())
            .await;

        let response = app(rooms)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .method("GET")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let body = String::from_utf8(body.to_vec()).expect("metrics body utf-8");

        assert!(body.contains("narwhal_rooms_total 2"));
        assert!(body.contains("narwhal_rooms{mode=\"meeting\"} 1"));
        assert!(body.contains("narwhal_rooms{mode=\"broadcast\"} 1"));
        assert!(body.contains("narwhal_meeting_joins_total 1"));
        assert!(body.contains("narwhal_meeting_publish_tracks_total 1"));
        assert!(body.contains("narwhal_meeting_participants_active 1"));
        assert!(body.contains("narwhal_meeting_publications_active 1"));
        assert!(body.contains(
            "narwhal_negotiation_total{cause=\"no_publisher\",flow=\"broadcast_whep_subscribe\",outcome=\"failure\"} 1"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ws_route_supports_renegotiation_across_publish_and_rejoin() {
        let rooms = manager();
        let (ws_url, server_handle) = match spawn_ws_app(rooms).await {
            Ok(value) => value,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(err) => panic!("ws app bind should succeed: {err}"),
        };

        let mut alice = ws_connect(&ws_url).await;
        let mut bob = ws_connect(&ws_url).await;

        let alice_join = ws_rpc(
            &mut alice,
            1,
            "join",
            json!({
                "room": "ws-integration",
                "display_name": "Alice"
            }),
        )
        .await;
        let alice_participant_id = alice_join["participant_id"]
            .as_str()
            .expect("alice participant id")
            .to_string();
        assert_eq!(alice_join["negotiation_state"], json!("awaiting_initial_offer"));

        let bob_join = ws_rpc(
            &mut bob,
            2,
            "join",
            json!({
                "room": "ws-integration",
                "display_name": "Bob"
            }),
        )
        .await;
        let bob_initial_revision = bob_join["revision"].as_u64().expect("bob revision");

        let bob_initial_offer = ws_rpc(
            &mut bob,
            3,
            "sdp_offer",
            json!({
                "revision": bob_initial_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await;
        assert!(
            bob_initial_offer["answer_sdp"]
                .as_str()
                .expect("answer sdp")
                .contains("m=audio")
        );
        assert_eq!(bob_initial_offer["negotiation_state"], json!("stable"));
        let bob_initial_trickle = ws_rpc(
            &mut bob,
            31,
            "trickle_ice",
            json!({
                "mline_index": 0,
                "candidate": "candidate:1 1 udp 2122260223 192.0.2.10 54400 typ host"
            }),
        )
        .await;
        assert_eq!(bob_initial_trickle["ok"], json!(true));
        let bob_initial_drain = ws_rpc(
            &mut bob,
            32,
            "drain_ice",
            json!({
                "max": 8
            }),
        )
        .await;
        assert!(bob_initial_drain["candidates"].is_array());

        let _alice_publish = ws_rpc(
            &mut alice,
            4,
            "publish_tracks",
            json!({
                "tracks": [
                    {
                        "track_id": "alice-audio-v1",
                        "media_kind": "audio",
                        "mid": "0"
                    },
                    {
                        "track_id": "alice-video-v1",
                        "media_kind": "video",
                        "mid": "1"
                    }
                ]
            }),
        )
        .await;

        let bob_subscribe = ws_rpc(
            &mut bob,
            5,
            "subscribe",
            json!({
                "track_ids": ["alice-audio-v1", "alice-video-v1"]
            }),
        )
        .await;
        let bob_subscribe_revision = bob_subscribe["revision"]
            .as_u64()
            .expect("subscribe revision");
        assert_eq!(bob_subscribe["needs_renegotiation"], json!(false));
        assert_eq!(bob_subscribe["negotiation_state"], json!("awaiting_initial_offer"));

        let bob_renegotiated = ws_rpc(
            &mut bob,
            6,
            "sdp_offer",
            json!({
                "revision": bob_subscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await;
        assert_eq!(bob_renegotiated["revision"], json!(bob_subscribe_revision));
        let bob_post_subscribe_trickle = ws_rpc(
            &mut bob,
            33,
            "trickle_ice",
            json!({
                "mline_index": 1,
                "candidate": "candidate:2 1 udp 2122260223 192.0.2.11 54402 typ host"
            }),
        )
        .await;
        assert_eq!(bob_post_subscribe_trickle["ok"], json!(true));
        let bob_post_subscribe_drain = ws_rpc(
            &mut bob,
            34,
            "drain_ice",
            json!({
                "max": 8
            }),
        )
        .await;
        assert!(bob_post_subscribe_drain["candidates"].is_array());

        let _alice_left = ws_rpc(&mut alice, 7, "leave", json!({})).await;
        alice.close(None).await.expect("alice close succeeds");

        let bob_after_leave = ws_rpc(&mut bob, 8, "list_subscriptions", json!({})).await;
        assert_eq!(bob_after_leave["requested_track_ids"], json!([]));
        assert_eq!(bob_after_leave["effective_track_ids"], json!([]));

        let mut alice_rejoined = ws_connect(&ws_url).await;
        let alice_rejoin = ws_rpc(
            &mut alice_rejoined,
            9,
            "join",
            json!({
                "room": "ws-integration",
                "display_name": "Alice Return"
            }),
        )
        .await;
        assert_ne!(
            alice_rejoin["participant_id"]
                .as_str()
                .expect("new participant id"),
            alice_participant_id
        );

        let _alice_republish = ws_rpc(
            &mut alice_rejoined,
            10,
            "publish_tracks",
            json!({
                "tracks": [
                    {
                        "track_id": "alice-audio-v2",
                        "media_kind": "audio",
                        "mid": "0"
                    }
                ]
            }),
        )
        .await;

        let bob_resubscribe = ws_rpc(
            &mut bob,
            11,
            "subscribe",
            json!({
                "track_ids": ["alice-audio-v2"]
            }),
        )
        .await;
        let bob_resubscribe_revision = bob_resubscribe["revision"]
            .as_u64()
            .expect("resubscribe revision");

        let bob_reoffer = ws_rpc(
            &mut bob,
            12,
            "sdp_offer",
            json!({
                "revision": bob_resubscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await;
        assert_eq!(bob_reoffer["revision"], json!(bob_resubscribe_revision));
        let bob_post_rejoin_trickle = ws_rpc(
            &mut bob,
            35,
            "trickle_ice",
            json!({
                "mline_index": 0,
                "candidate": "candidate:3 1 udp 2122260223 192.0.2.12 54404 typ host"
            }),
        )
        .await;
        assert_eq!(bob_post_rejoin_trickle["ok"], json!(true));
        let bob_post_rejoin_drain = ws_rpc(
            &mut bob,
            36,
            "drain_ice",
            json!({
                "max": 8
            }),
        )
        .await;
        assert!(bob_post_rejoin_drain["candidates"].is_array());

        let bob_final_subscriptions = ws_rpc(&mut bob, 13, "list_subscriptions", json!({})).await;
        assert_eq!(
            bob_final_subscriptions["requested_track_ids"],
            json!(["alice-audio-v2"])
        );
        assert_eq!(
            bob_final_subscriptions["effective_track_ids"],
            json!(["alice-audio-v2"])
        );

        bob.close(None).await.expect("bob close succeeds");
        alice_rejoined
            .close(None)
            .await
            .expect("alice rejoined close succeeds");
        server_handle.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ws_route_pushes_room_wide_renegotiation_notifications() {
        let rooms = manager();
        let (ws_url, server_handle) = match spawn_ws_app(rooms).await {
            Ok(value) => value,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(err) => panic!("ws app bind should succeed: {err}"),
        };

        let mut alice = ws_connect(&ws_url).await;
        let mut bob = ws_connect(&ws_url).await;

        let _alice_join = ws_rpc(
            &mut alice,
            1,
            "join",
            json!({
                "room": "ws-room-wide-renegotiation",
                "display_name": "Alice"
            }),
        )
        .await;

        let bob_join = ws_rpc(
            &mut bob,
            2,
            "join",
            json!({
                "room": "ws-room-wide-renegotiation",
                "display_name": "Bob"
            }),
        )
        .await;
        let bob_join_revision = bob_join["revision"].as_u64().expect("bob join revision");

        let _alice_publish = ws_rpc(
            &mut alice,
            3,
            "publish_tracks",
            json!({
                "tracks": [
                    {
                        "track_id": "alice-audio-v1",
                        "media_kind": "audio",
                        "mid": "0"
                    }
                ]
            }),
        )
        .await;

        let _bob_initial_offer = ws_rpc(
            &mut bob,
            4,
            "sdp_offer",
            json!({
                "revision": bob_join_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await;

        let bob_subscribe = ws_rpc(
            &mut bob,
            5,
            "subscribe",
            json!({
                "track_ids": ["alice-audio-v1"]
            }),
        )
        .await;
        assert_eq!(bob_subscribe["needs_renegotiation"], json!(true));
        let bob_subscribe_revision = bob_subscribe["revision"]
            .as_u64()
            .expect("bob subscribe revision");

        let _bob_reoffer = ws_rpc(
            &mut bob,
            6,
            "sdp_offer",
            json!({
                "revision": bob_subscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await;

        let alice_unpublish = ws_rpc(
            &mut alice,
            7,
            "unpublish_tracks",
            json!({
                "track_ids": ["alice-audio-v1"]
            }),
        )
        .await;
        assert_eq!(alice_unpublish["needs_renegotiation"], json!(false));

        let notification = ws_next_json(&mut bob).await;
        assert_eq!(notification["jsonrpc"], json!("2.0"));
        assert_eq!(notification["method"], json!("renegotiation_required"));
        assert_eq!(
            notification["params"]["reason"],
            json!("tracks_unpublished")
        );
        assert_eq!(
            notification["params"]["negotiation_state"],
            json!("renegotiation_required")
        );
        assert_eq!(
            notification["params"]["revision"],
            alice_unpublish["revision"]
        );

        let bob_state_after_notification = ws_rpc(&mut bob, 8, "list_subscriptions", json!({})).await;
        assert_eq!(bob_state_after_notification["requested_track_ids"], json!([]));
        assert_eq!(bob_state_after_notification["effective_track_ids"], json!([]));

        alice.close(None).await.expect("alice close succeeds");
        bob.close(None).await.expect("bob close succeeds");
        server_handle.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ws_upgrade_returns_service_unavailable_when_draining() {
        let state = AppState::new(manager());
        state.mark_draining();

        let response = app_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/ws")
                    .method("GET")
                    .header(header::CONNECTION, "upgrade")
                    .header(header::UPGRADE, "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("response available");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json,
            json!({ "error": "server is draining and not accepting new sessions" })
        );
    }
}
