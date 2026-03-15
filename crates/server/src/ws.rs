use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use narwhal_core::{
    Error as CoreError, MediaKind, MeetingPolicyMode, MeetingPublishTrack, RoomId, RoomManager,
    RoomMode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};
use uuid::Uuid;

use crate::AppState;
use crate::errors::TransportError;

pub async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state.rooms()))
}

struct SessionState {
    rooms: RoomManager,
    joined: Option<JoinedParticipant>,
}

struct JoinedParticipant {
    room: RoomId,
    participant_id: String,
    revision: u64,
}

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

#[derive(Deserialize)]
struct JoinParams {
    room: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct SdpOfferParams {
    revision: u64,
    offer_sdp: String,
}

#[derive(Deserialize)]
struct TrickleIceParams {
    mline_index: u32,
    candidate: String,
}

#[derive(Deserialize)]
struct DrainIceParams {
    #[serde(default = "default_drain_max")]
    max: usize,
}

fn default_drain_max() -> usize {
    50
}

#[derive(Deserialize)]
struct PublishTracksParams {
    tracks: Vec<PublishTrackParam>,
}

#[derive(Deserialize)]
struct PublishTrackParam {
    track_id: String,
    media_kind: WireMediaKind,
    #[serde(default)]
    mid: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireMediaKind {
    Audio,
    Video,
}

impl From<WireMediaKind> for MediaKind {
    fn from(value: WireMediaKind) -> Self {
        match value {
            WireMediaKind::Audio => MediaKind::Audio,
            WireMediaKind::Video => MediaKind::Video,
        }
    }
}

#[derive(Deserialize)]
struct TrackIdsParams {
    track_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireMeetingPolicyMode {
    Standard,
    LowBandwidth,
}

impl From<WireMeetingPolicyMode> for MeetingPolicyMode {
    fn from(value: WireMeetingPolicyMode) -> Self {
        match value {
            WireMeetingPolicyMode::Standard => MeetingPolicyMode::Standard,
            WireMeetingPolicyMode::LowBandwidth => MeetingPolicyMode::LowBandwidth,
        }
    }
}

fn policy_mode_label(mode: MeetingPolicyMode) -> &'static str {
    match mode {
        MeetingPolicyMode::Standard => "standard",
        MeetingPolicyMode::LowBandwidth => "low_bandwidth",
    }
}

#[derive(Deserialize)]
struct SetPolicyModeParams {
    mode: WireMeetingPolicyMode,
}

async fn handle_socket(mut socket: WebSocket, rooms: RoomManager) {
    let mut state = SessionState {
        rooms,
        joined: None,
    };

    while let Some(msg) = socket.recv().await {
        let Ok(msg) = msg else {
            break;
        };

        match msg {
            Message::Text(text) => {
                if let Some(resp) = handle_text(&mut state, text.to_string()).await {
                    let Ok(serialized) = serde_json::to_string(&resp) else {
                        break;
                    };
                    if socket.send(Message::Text(serialized.into())).await.is_err() {
                        break;
                    }
                }
            }
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) => {}
        }
    }

    cleanup_joined_session(&mut state).await;
}

async fn cleanup_joined_session(state: &mut SessionState) {
    if let Some(joined) = state.joined.take() {
        if let Err(err) = state
            .rooms
            .meeting_leave(joined.room.clone(), &joined.participant_id)
            .await
        {
            warn!(
                participant_id = %joined.participant_id,
                room = %joined.room.0,
                cause = TransportError::cause_label_from_anyhow(&err),
                "meeting ws close cleanup failed: {err:#}"
            );
        } else {
            info!(
                participant_id = %joined.participant_id,
                room = %joined.room.0,
                "meeting ws connection closed; participant left session"
            );
        }
    }
}

async fn handle_text(state: &mut SessionState, text: String) -> Option<RpcResponse> {
    let req = match serde_json::from_str::<RpcRequest>(&text) {
        Ok(v) => v,
        Err(err) => {
            return Some(error_response(None, -32700, format!("parse error: {err}")));
        }
    };

    let id = req.id.clone();
    if req.jsonrpc != "2.0" {
        return Some(error_response(
            id,
            -32600,
            "invalid jsonrpc version".to_string(),
        ));
    }

    let resp = match req.method.as_str() {
        "join" => join(state, req.params).await,
        "leave" => leave(state).await,
        "sdp_offer" => sdp_offer(state, req.params).await,
        "trickle_ice" => trickle_ice(state, req.params).await,
        "drain_ice" => drain_ice(state, req.params).await,
        "list_participants" => list_participants(state).await,
        "list_publications" => list_publications(state).await,
        "list_streams" => list_streams(state).await,
        "list_subscriptions" => list_subscriptions(state).await,
        "get_policy_mode" => get_policy_mode(state).await,
        "set_policy_mode" => set_policy_mode(state, req.params).await,
        "publish_tracks" => publish_tracks(state, req.params).await,
        "unpublish_tracks" => unpublish_tracks(state, req.params).await,
        "subscribe" => subscribe(state, req.params).await,
        "unsubscribe" => unsubscribe(state, req.params).await,
        _ => Err(rpc_error(-32601, "method not found".to_string())),
    };

    if id.is_none() {
        return None;
    }

    Some(match resp {
        Ok(result) => success_response(id, result),
        Err(err) => error_response(id, err.code, err.message),
    })
}

async fn join(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    if state.joined.is_some() {
        return Err(rpc_core_error(CoreError::AlreadyJoined));
    }

    let params: JoinParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for join: {err}")))?;

    let room = RoomId(params.room);
    let participant_id = Uuid::new_v4().to_string();
    let revision = match state.rooms.room_mode(&room) {
        Some(RoomMode::Broadcast) => {
            return Err(rpc_core_error(
                CoreError::MeetingSignalingUnavailableInBroadcastMode,
            ));
        }
        Some(RoomMode::Meeting) => state
            .rooms
            .meeting_join(room.clone(), participant_id.clone(), params.display_name)
            .map_err(rpc_transport_error)?,
        None => {
            state
                .rooms
                .ensure_room_mode(room.clone(), RoomMode::Meeting);
            state
                .rooms
                .meeting_join(room.clone(), participant_id.clone(), params.display_name)
                .map_err(rpc_transport_error)?
        }
    };

    state.joined = Some(JoinedParticipant {
        room: room.clone(),
        participant_id: participant_id.clone(),
        revision,
    });

    info!(
        participant_id = %participant_id,
        room = %room.0,
        "participant joined meeting ws session"
    );

    Ok(json!({
        "participant_id": participant_id,
        "room": room.0,
        "mode": "meeting",
        "revision": revision
    }))
}

async fn leave(state: &mut SessionState) -> Result<Value, RpcError> {
    let Some(joined) = state.joined.take() else {
        return Err(rpc_core_error(CoreError::NotJoined));
    };

    state
        .rooms
        .meeting_leave(joined.room.clone(), &joined.participant_id)
        .await
        .map_err(rpc_transport_error)?;

    info!(
        participant_id = %joined.participant_id,
        room = %joined.room.0,
        "participant left meeting ws session"
    );

    Ok(json!({
        "left": true
    }))
}

async fn sdp_offer(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_mut()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: SdpOfferParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for sdp_offer: {err}")))?;

    let answer = state
        .rooms
        .meeting_sdp_offer(
            joined.room.clone(),
            &joined.participant_id,
            params.revision,
            params.offer_sdp,
        )
        .await
        .map_err(rpc_transport_error)?;

    joined.revision = answer.revision;

    Ok(json!({
        "answer_sdp": answer.answer_sdp,
        "revision": answer.revision
    }))
}

async fn trickle_ice(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: TrickleIceParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for trickle_ice: {err}")))?;

    state
        .rooms
        .meeting_trickle(
            joined.room.clone(),
            &joined.participant_id,
            params.mline_index,
            params.candidate,
        )
        .await
        .map_err(rpc_transport_error)?;

    Ok(json!({ "ok": true }))
}

async fn drain_ice(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: DrainIceParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for drain_ice: {err}")))?;

    let list = state
        .rooms
        .meeting_drain_ice(joined.room.clone(), &joined.participant_id, params.max)
        .map_err(rpc_transport_error)?;

    Ok(json!({
        "candidates": list
            .into_iter()
            .map(|c| json!({"mline_index": c.mline_index, "candidate": c.candidate}))
            .collect::<Vec<_>>()
    }))
}

async fn publish_tracks(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_mut()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: PublishTracksParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for publish_tracks: {err}")))?;
    let tracks = params
        .tracks
        .into_iter()
        .map(|t| MeetingPublishTrack {
            track_id: t.track_id,
            media_kind: t.media_kind.into(),
            mid: t.mid,
        })
        .collect();

    let revision = state
        .rooms
        .meeting_publish_tracks(joined.room.clone(), &joined.participant_id, tracks)
        .map_err(rpc_transport_error)?;
    joined.revision = revision;

    Ok(json!({
        "revision": revision,
        "needs_renegotiation": false
    }))
}

async fn unpublish_tracks(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_mut()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: TrackIdsParams = serde_json::from_value(params).map_err(|err| {
        rpc_error(
            -32602,
            format!("invalid params for unpublish_tracks: {err}"),
        )
    })?;

    let revision = state
        .rooms
        .meeting_unpublish_tracks(
            joined.room.clone(),
            &joined.participant_id,
            params.track_ids,
        )
        .map_err(rpc_transport_error)?;
    joined.revision = revision;

    Ok(json!({
        "revision": revision,
        "needs_renegotiation": false
    }))
}

async fn subscribe(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_mut()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: TrackIdsParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for subscribe: {err}")))?;

    let revision = state
        .rooms
        .meeting_subscribe(
            joined.room.clone(),
            &joined.participant_id,
            params.track_ids,
        )
        .map_err(rpc_transport_error)?;
    joined.revision = revision;

    Ok(json!({
        "revision": revision,
        "needs_renegotiation": false
    }))
}

async fn unsubscribe(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_mut()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: TrackIdsParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for unsubscribe: {err}")))?;

    let revision = state
        .rooms
        .meeting_unsubscribe(
            joined.room.clone(),
            &joined.participant_id,
            params.track_ids,
        )
        .map_err(rpc_transport_error)?;
    joined.revision = revision;

    Ok(json!({
        "revision": revision,
        "needs_renegotiation": false
    }))
}

async fn list_publications(state: &mut SessionState) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;

    let list = state
        .rooms
        .meeting_list_publications(joined.room.clone())
        .map_err(rpc_transport_error)?;

    Ok(json!({
        "publications": list
            .into_iter()
            .map(|p| {
                json!({
                    "track_id": p.track_id,
                    "publisher_id": p.publisher_id,
                    "media_kind": match p.media_kind {
                        MediaKind::Audio => "audio",
                        MediaKind::Video => "video",
                    },
                    "mid": p.mid
                })
            })
            .collect::<Vec<_>>()
    }))
}

async fn list_streams(state: &mut SessionState) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;

    let list = state
        .rooms
        .meeting_list_streams(joined.room.clone())
        .map_err(rpc_transport_error)?;

    Ok(json!({
        "streams": list
            .into_iter()
            .map(|stream| {
                json!({
                    "track_id": stream.track_id,
                    "publisher_id": stream.publisher_id,
                    "media_kind": match stream.media_kind {
                        MediaKind::Audio => "audio",
                        MediaKind::Video => "video",
                    },
                    "ssrc": stream.ssrc,
                    "encoding_id": stream.encoding_id,
                    "mid": stream.mid,
                    "rid": stream.rid,
                    "spatial_layer": stream.spatial_layer
                })
            })
            .collect::<Vec<_>>()
    }))
}

async fn list_participants(state: &mut SessionState) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;

    let list = state
        .rooms
        .meeting_list_participants(joined.room.clone())
        .map_err(rpc_transport_error)?;

    Ok(json!({
        "participants": list
            .into_iter()
            .map(|p| {
                json!({
                    "participant_id": p.participant_id,
                    "display_name": p.display_name
                })
            })
            .collect::<Vec<_>>()
    }))
}

async fn list_subscriptions(state: &mut SessionState) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;

    let requested_track_ids = state
        .rooms
        .meeting_list_subscription_requests(joined.room.clone(), &joined.participant_id)
        .map_err(rpc_transport_error)?;
    let effective_track_ids = state
        .rooms
        .meeting_list_effective_subscriptions(joined.room.clone(), &joined.participant_id)
        .map_err(rpc_transport_error)?;

    Ok(json!({
        "requested_track_ids": requested_track_ids,
        "effective_track_ids": effective_track_ids,
        "track_ids": effective_track_ids
    }))
}

async fn get_policy_mode(state: &mut SessionState) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;

    let mode = state
        .rooms
        .meeting_policy_mode(joined.room.clone())
        .map_err(rpc_transport_error)?;

    Ok(json!({
        "mode": policy_mode_label(mode)
    }))
}

async fn set_policy_mode(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_mut()
        .ok_or_else(|| rpc_core_error(CoreError::NotJoined))?;
    let params: SetPolicyModeParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for set_policy_mode: {err}")))?;

    let mode: MeetingPolicyMode = params.mode.into();
    let revision = state
        .rooms
        .meeting_set_policy_mode(joined.room.clone(), mode)
        .map_err(rpc_transport_error)?;
    joined.revision = revision;

    Ok(json!({
        "mode": policy_mode_label(mode),
        "revision": revision,
        "needs_renegotiation": false
    }))
}

fn success_response(id: Option<Value>, result: Value) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn error_response(id: Option<Value>, code: i32, message: String) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError { code, message }),
    }
}

fn rpc_error(code: i32, message: String) -> RpcError {
    RpcError { code, message }
}

fn rpc_transport_error(err: anyhow::Error) -> RpcError {
    let err = TransportError::from_anyhow(err);
    rpc_error(err.rpc_code(), err.message)
}

fn rpc_core_error(err: CoreError) -> RpcError {
    rpc_transport_error(err.into())
}

#[cfg(test)]
mod tests {
    use super::{
        JoinedParticipant, SessionState, cleanup_joined_session, handle_text, join, leave,
        list_participants, list_subscriptions, publish_tracks, sdp_offer, subscribe, unsubscribe,
    };
    use media::GstRuntime;
    use narwhal_core::{MediaKind, MeetingPublishTrack, RoomId, RoomManager};
    use serde_json::json;

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

    #[tokio::test(flavor = "current_thread")]
    async fn join_rejects_when_session_already_joined() {
        let mut state = SessionState {
            rooms: manager(),
            joined: Some(JoinedParticipant {
                room: RoomId("room-a".to_string()),
                participant_id: "participant-a".to_string(),
                revision: 1,
            }),
        };

        let err = join(
            &mut state,
            json!({
                "room": "room-b",
                "display_name": "Bob"
            }),
        )
        .await
        .expect_err("join should fail");

        assert_eq!(err.code, 4090);
        assert_eq!(err.message, "already joined; leave first or reconnect");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn leave_requires_joined_session() {
        let mut state = SessionState {
            rooms: manager(),
            joined: None,
        };

        let err = leave(&mut state).await.expect_err("leave should fail");
        assert_eq!(err.code, 4220);
        assert_eq!(err.message, "not joined");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_text_maps_not_joined_rpc_error() {
        let mut state = SessionState {
            rooms: manager(),
            joined: None,
        };

        let response = handle_text(
            &mut state,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "leave",
                "params": {}
            })
            .to_string(),
        )
        .await
        .expect("response expected");

        let error = response.error.expect("rpc error expected");
        assert_eq!(error.code, 4220);
        assert_eq!(error.message, "not joined");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn meeting_mutations_increment_revision() {
        let rooms = manager();
        let room = RoomId("meeting-revision".to_string());
        let alice_revision = rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        let bob_revision = rooms
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");

        let mut state = SessionState {
            rooms,
            joined: Some(JoinedParticipant {
                room: room.clone(),
                participant_id: "alice".to_string(),
                revision: alice_revision,
            }),
        };

        let publish_result = publish_tracks(
            &mut state,
            json!({
                "tracks": [
                    {
                        "track_id": "alice-audio",
                        "media_kind": "audio",
                        "mid": "0"
                    }
                ]
            }),
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "publish should succeed: code={} message={}",
                err.code, err.message
            )
        });
        let publish_revision = publish_result["revision"]
            .as_u64()
            .expect("revision present");
        assert!(publish_revision > bob_revision);

        state.joined = Some(JoinedParticipant {
            room,
            participant_id: "bob".to_string(),
            revision: bob_revision,
        });

        let subscribe_result = subscribe(
            &mut state,
            json!({
                "track_ids": ["alice-audio"]
            }),
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "subscribe should succeed: code={} message={}",
                err.code, err.message
            )
        });
        let subscribe_revision = subscribe_result["revision"]
            .as_u64()
            .expect("revision present");
        assert!(subscribe_revision > publish_revision);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sdp_offer_rejects_future_revision_before_negotiation() {
        let rooms = manager();
        let room = RoomId("meeting-future-revision".to_string());
        let revision = rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");

        let mut state = SessionState {
            rooms,
            joined: Some(JoinedParticipant {
                room,
                participant_id: "alice".to_string(),
                revision,
            }),
        };

        let err = sdp_offer(
            &mut state,
            json!({
                "revision": revision + 100,
                "offer_sdp": "v=0\r\n"
            }),
        )
        .await
        .expect_err("future revision should fail");

        assert_eq!(err.code, 4220);
        assert_eq!(err.message, "invalid future revision: got 101, current 1");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sdp_offer_accepts_current_revision_with_browser_like_offer() {
        let rooms = manager();
        let room = RoomId("meeting-current-sdp".to_string());
        let revision = rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");

        let mut state = SessionState {
            rooms,
            joined: Some(JoinedParticipant {
                room,
                participant_id: "alice".to_string(),
                revision,
            }),
        };

        let result = sdp_offer(
            &mut state,
            json!({
                "revision": revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await
        .expect("current revision offer should negotiate");

        assert!(
            result["answer_sdp"]
                .as_str()
                .expect("answer sdp present")
                .contains("m=audio")
        );
        assert_eq!(result["revision"], json!(revision));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sdp_offer_renegotiates_after_publish_and_subscribe() {
        let rooms = manager();
        let room = RoomId("meeting-renegotiate-subscribe".to_string());
        let alice_revision = rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        let bob_revision = rooms
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");

        let mut bob_state = SessionState {
            rooms: rooms.clone(),
            joined: Some(JoinedParticipant {
                room: room.clone(),
                participant_id: "bob".to_string(),
                revision: bob_revision,
            }),
        };

        sdp_offer(
            &mut bob_state,
            json!({
                "revision": bob_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await
        .expect("initial bob negotiation should succeed");

        let publish_revision = rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![
                    MeetingPublishTrack {
                        track_id: "alice-audio".to_string(),
                        media_kind: MediaKind::Audio,
                        mid: Some("0".to_string()),
                    },
                    MeetingPublishTrack {
                        track_id: "alice-video".to_string(),
                        media_kind: MediaKind::Video,
                        mid: Some("1".to_string()),
                    },
                ],
            )
            .expect("alice publishes");
        assert!(publish_revision > alice_revision);

        let subscribe_result = subscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-audio", "alice-video"]
            }),
        )
        .await
        .expect("bob subscribes");
        let subscribe_revision = subscribe_result["revision"]
            .as_u64()
            .expect("revision present");
        assert!(subscribe_revision > publish_revision);

        let renegotiated = sdp_offer(
            &mut bob_state,
            json!({
                "revision": subscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await
        .expect("renegotiation after subscribe should succeed");

        assert_eq!(renegotiated["revision"], json!(subscribe_revision));
        assert!(
            renegotiated["answer_sdp"]
                .as_str()
                .expect("answer sdp present")
                .contains("m=video")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sdp_offer_renegotiates_after_unsubscribe() {
        let rooms = manager();
        let room = RoomId("meeting-renegotiate-unsubscribe".to_string());
        rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        let bob_revision = rooms
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");

        rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![
                    MeetingPublishTrack {
                        track_id: "alice-audio".to_string(),
                        media_kind: MediaKind::Audio,
                        mid: Some("0".to_string()),
                    },
                    MeetingPublishTrack {
                        track_id: "alice-video".to_string(),
                        media_kind: MediaKind::Video,
                        mid: Some("1".to_string()),
                    },
                ],
            )
            .expect("alice publishes");

        let mut bob_state = SessionState {
            rooms,
            joined: Some(JoinedParticipant {
                room,
                participant_id: "bob".to_string(),
                revision: bob_revision,
            }),
        };

        let subscribe_result = subscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-audio", "alice-video"]
            }),
        )
        .await
        .expect("bob subscribes");
        let subscribe_revision = subscribe_result["revision"]
            .as_u64()
            .expect("revision present");

        sdp_offer(
            &mut bob_state,
            json!({
                "revision": subscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await
        .expect("negotiation after subscribe should succeed");

        let unsubscribe_result = unsubscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-video"]
            }),
        )
        .await
        .expect("bob unsubscribes");
        let unsubscribe_revision = unsubscribe_result["revision"]
            .as_u64()
            .expect("revision present");
        assert!(unsubscribe_revision > subscribe_revision);

        let renegotiated = sdp_offer(
            &mut bob_state,
            json!({
                "revision": unsubscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await
        .expect("renegotiation after unsubscribe should succeed");

        assert_eq!(renegotiated["revision"], json!(unsubscribe_revision));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sdp_offer_renegotiates_after_publisher_rejoin_and_resubscribe() {
        let rooms = manager();
        let room = RoomId("meeting-renegotiate-rejoin".to_string());
        rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        let bob_revision = rooms
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");

        rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![MeetingPublishTrack {
                    track_id: "alice-audio-v1".to_string(),
                    media_kind: MediaKind::Audio,
                    mid: Some("0".to_string()),
                }],
            )
            .expect("alice publishes");

        let mut bob_state = SessionState {
            rooms: rooms.clone(),
            joined: Some(JoinedParticipant {
                room: room.clone(),
                participant_id: "bob".to_string(),
                revision: bob_revision,
            }),
        };

        let first_subscribe = subscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-audio-v1"]
            }),
        )
        .await
        .expect("bob subscribes to first track");
        let first_subscribe_revision = first_subscribe["revision"]
            .as_u64()
            .expect("revision present");

        sdp_offer(
            &mut bob_state,
            json!({
                "revision": first_subscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await
        .expect("initial negotiation should succeed");

        rooms
            .meeting_leave(room.clone(), "alice")
            .await
            .expect("alice leaves");
        let rejoin_revision = rooms
            .meeting_join(
                room.clone(),
                "alice".to_string(),
                Some("Alice Return".to_string()),
            )
            .expect("alice rejoins");
        let republish_revision = rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![MeetingPublishTrack {
                    track_id: "alice-audio-v2".to_string(),
                    media_kind: MediaKind::Audio,
                    mid: Some("0".to_string()),
                }],
            )
            .expect("alice republishes");
        assert!(republish_revision > rejoin_revision);

        let second_subscribe = subscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-audio-v2"]
            }),
        )
        .await
        .expect("bob resubscribes to second track");
        let second_subscribe_revision = second_subscribe["revision"]
            .as_u64()
            .expect("revision present");

        let renegotiated = sdp_offer(
            &mut bob_state,
            json!({
                "revision": second_subscribe_revision,
                "offer_sdp": browser_like_offer_sdp()
            }),
        )
        .await
        .expect("renegotiation after publisher rejoin should succeed");

        assert_eq!(renegotiated["revision"], json!(second_subscribe_revision));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscribe_and_unsubscribe_update_revision_and_visible_subscriptions() {
        let rooms = manager();
        let room = RoomId("meeting-subscriptions".to_string());
        let alice_revision = rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        let bob_revision = rooms
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");
        assert!(bob_revision >= alice_revision);
        rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![
                    MeetingPublishTrack {
                        track_id: "alice-audio".to_string(),
                        media_kind: MediaKind::Audio,
                        mid: Some("0".to_string()),
                    },
                    MeetingPublishTrack {
                        track_id: "alice-video".to_string(),
                        media_kind: MediaKind::Video,
                        mid: Some("1".to_string()),
                    },
                ],
            )
            .expect("alice publishes");

        let mut state = SessionState {
            rooms,
            joined: Some(JoinedParticipant {
                room: room.clone(),
                participant_id: "bob".to_string(),
                revision: bob_revision,
            }),
        };

        let subscribe_result = subscribe(
            &mut state,
            json!({
                "track_ids": ["alice-audio", "alice-video"]
            }),
        )
        .await
        .expect("subscribe should succeed");
        let subscribe_revision = subscribe_result["revision"]
            .as_u64()
            .expect("revision present");
        assert_eq!(subscribe_result["needs_renegotiation"], json!(false));

        let subscriptions_after_subscribe = list_subscriptions(&mut state)
            .await
            .expect("subscriptions should be listable after subscribe");
        assert_eq!(
            subscriptions_after_subscribe["requested_track_ids"],
            json!(["alice-audio", "alice-video"])
        );
        assert_eq!(
            subscriptions_after_subscribe["effective_track_ids"],
            json!(["alice-audio", "alice-video"])
        );

        let unsubscribe_result = unsubscribe(
            &mut state,
            json!({
                "track_ids": ["alice-video"]
            }),
        )
        .await
        .expect("unsubscribe should succeed");
        let unsubscribe_revision = unsubscribe_result["revision"]
            .as_u64()
            .expect("revision present");
        assert!(unsubscribe_revision > subscribe_revision);
        assert_eq!(unsubscribe_result["needs_renegotiation"], json!(false));

        let subscriptions_after_unsubscribe = list_subscriptions(&mut state)
            .await
            .expect("subscriptions should be listable after unsubscribe");
        assert_eq!(
            subscriptions_after_unsubscribe["requested_track_ids"],
            json!(["alice-audio"])
        );
        assert_eq!(
            subscriptions_after_unsubscribe["effective_track_ids"],
            json!(["alice-audio"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ws_close_cleanup_removes_participant_and_allows_reconnect() {
        let rooms = manager();
        let room = RoomId("meeting-reconnect".to_string());

        let first_join = join(
            &mut SessionState {
                rooms: rooms.clone(),
                joined: None,
            },
            json!({
                "room": room.0,
                "display_name": "Alice"
            }),
        )
        .await
        .expect("first join should succeed");

        let first_participant_id = first_join["participant_id"]
            .as_str()
            .expect("participant id present")
            .to_string();
        let first_revision = first_join["revision"].as_u64().expect("revision present");

        let mut first_state = SessionState {
            rooms: rooms.clone(),
            joined: Some(JoinedParticipant {
                room: room.clone(),
                participant_id: first_participant_id.clone(),
                revision: first_revision,
            }),
        };

        let before_cleanup = list_participants(&mut first_state)
            .await
            .expect("participants should be listable before cleanup");
        assert_eq!(
            before_cleanup["participants"].as_array().map(Vec::len),
            Some(1)
        );

        cleanup_joined_session(&mut first_state).await;
        assert!(first_state.joined.is_none());

        let mut second_state = SessionState {
            rooms,
            joined: None,
        };
        let second_join = join(
            &mut second_state,
            json!({
                "room": room.0,
                "display_name": "Alice Reconnected"
            }),
        )
        .await
        .expect("reconnect join should succeed");
        let second_participant_id = second_join["participant_id"]
            .as_str()
            .expect("participant id present");
        assert_ne!(second_participant_id, first_participant_id);

        let after_reconnect = list_participants(&mut second_state)
            .await
            .expect("participants should be listable after reconnect");
        let participants = after_reconnect["participants"]
            .as_array()
            .expect("participants array");
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[0]["display_name"], json!("Alice Reconnected"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publisher_leave_prunes_other_participant_subscriptions() {
        let rooms = manager();
        let room = RoomId("meeting-publisher-leave".to_string());
        let alice_revision = rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        let bob_revision = rooms
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");
        assert!(bob_revision >= alice_revision);

        rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![
                    MeetingPublishTrack {
                        track_id: "alice-audio".to_string(),
                        media_kind: MediaKind::Audio,
                        mid: Some("0".to_string()),
                    },
                    MeetingPublishTrack {
                        track_id: "alice-video".to_string(),
                        media_kind: MediaKind::Video,
                        mid: Some("1".to_string()),
                    },
                ],
            )
            .expect("alice publishes");

        let mut bob_state = SessionState {
            rooms: rooms.clone(),
            joined: Some(JoinedParticipant {
                room: room.clone(),
                participant_id: "bob".to_string(),
                revision: bob_revision,
            }),
        };

        subscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-audio", "alice-video"]
            }),
        )
        .await
        .expect("bob subscribes");

        let before_leave = list_subscriptions(&mut bob_state)
            .await
            .expect("subscriptions should be visible before leave");
        assert_eq!(
            before_leave["requested_track_ids"],
            json!(["alice-audio", "alice-video"])
        );

        rooms
            .meeting_leave(room.clone(), "alice")
            .await
            .expect("alice leaves");

        let after_leave = list_subscriptions(&mut bob_state)
            .await
            .expect("subscriptions should still be queryable after publisher leave");
        assert_eq!(after_leave["requested_track_ids"], json!([]));
        assert_eq!(after_leave["effective_track_ids"], json!([]));

        let publications = super::list_publications(&mut bob_state)
            .await
            .expect("publications should be listable after publisher leave");
        assert_eq!(publications["publications"], json!([]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publisher_rejoin_can_republish_and_be_resubscribed() {
        let rooms = manager();
        let room = RoomId("meeting-publisher-rejoin".to_string());
        let _alice_revision = rooms
            .meeting_join(room.clone(), "alice".to_string(), Some("Alice".to_string()))
            .expect("alice joins");
        let bob_revision = rooms
            .meeting_join(room.clone(), "bob".to_string(), Some("Bob".to_string()))
            .expect("bob joins");

        rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![MeetingPublishTrack {
                    track_id: "alice-audio-v1".to_string(),
                    media_kind: MediaKind::Audio,
                    mid: Some("0".to_string()),
                }],
            )
            .expect("alice publishes first track");

        let mut bob_state = SessionState {
            rooms: rooms.clone(),
            joined: Some(JoinedParticipant {
                room: room.clone(),
                participant_id: "bob".to_string(),
                revision: bob_revision,
            }),
        };

        let first_subscribe = subscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-audio-v1"]
            }),
        )
        .await
        .expect("bob subscribes to first publication");
        let first_subscribe_revision = first_subscribe["revision"]
            .as_u64()
            .expect("revision present");

        rooms
            .meeting_leave(room.clone(), "alice")
            .await
            .expect("alice leaves");

        let rejoin_revision = rooms
            .meeting_join(
                room.clone(),
                "alice".to_string(),
                Some("Alice Return".to_string()),
            )
            .expect("alice rejoins");
        assert!(rejoin_revision > first_subscribe_revision);

        let republish_revision = rooms
            .meeting_publish_tracks(
                room.clone(),
                "alice",
                vec![MeetingPublishTrack {
                    track_id: "alice-audio-v2".to_string(),
                    media_kind: MediaKind::Audio,
                    mid: Some("0".to_string()),
                }],
            )
            .expect("alice republishes");
        assert!(republish_revision > rejoin_revision);

        let second_subscribe = subscribe(
            &mut bob_state,
            json!({
                "track_ids": ["alice-audio-v2"]
            }),
        )
        .await
        .expect("bob resubscribes to new publication");
        let second_subscribe_revision = second_subscribe["revision"]
            .as_u64()
            .expect("revision present");
        assert!(second_subscribe_revision > republish_revision);
        assert_eq!(second_subscribe["needs_renegotiation"], json!(false));

        let subscriptions = list_subscriptions(&mut bob_state)
            .await
            .expect("subscriptions should be listable after republish");
        assert_eq!(
            subscriptions["requested_track_ids"],
            json!(["alice-audio-v2"])
        );
        assert_eq!(
            subscriptions["effective_track_ids"],
            json!(["alice-audio-v2"])
        );
    }
}
