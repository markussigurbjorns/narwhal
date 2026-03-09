use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use narwhal_core::{
    MeetingPolicyMode, MeetingPublishTrack, MediaKind, RoomId, RoomManager, RoomMode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};
use uuid::Uuid;

pub async fn ws_upgrade(ws: WebSocketUpgrade, State(rooms): State<RoomManager>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, rooms))
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

#[derive(Serialize)]
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
    let mut state = SessionState { rooms, joined: None };

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

    if let Some(joined) = state.joined.take() {
        if let Err(err) = state
            .rooms
            .meeting_leave(joined.room.clone(), &joined.participant_id)
            .await
        {
            warn!(
                participant_id = %joined.participant_id,
                room = %joined.room.0,
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
            return Some(error_response(
                None,
                -32700,
                format!("parse error: {err}"),
            ));
        }
    };

    let id = req.id.clone();
    if req.jsonrpc != "2.0" {
        return Some(error_response(id, -32600, "invalid jsonrpc version".to_string()));
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
        return Err(rpc_error(
            4001,
            "already joined; leave first or reconnect".to_string(),
        ));
    }

    let params: JoinParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for join: {err}")))?;

    let room = RoomId(params.room);
    let participant_id = Uuid::new_v4().to_string();
    let revision = match state.rooms.room_mode(&room) {
        Some(RoomMode::Broadcast) => {
            return Err(rpc_error(
                4002,
                "room already used in broadcast mode".to_string(),
            ));
        }
        Some(RoomMode::Meeting) => state
            .rooms
            .meeting_join(room.clone(), participant_id.clone(), params.display_name)
            .map_err(|err| rpc_error(5001, err.to_string()))?,
        None => {
            state.rooms.ensure_room_mode(room.clone(), RoomMode::Meeting);
            state
                .rooms
                .meeting_join(room.clone(), participant_id.clone(), params.display_name)
                .map_err(|err| rpc_error(5001, err.to_string()))?
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
        return Err(rpc_error(4003, "not joined".to_string()));
    };

    state
        .rooms
        .meeting_leave(joined.room.clone(), &joined.participant_id)
        .await
        .map_err(|err| rpc_error(5002, err.to_string()))?;

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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
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
        .map_err(|err| rpc_error(4090, err.to_string()))?;

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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
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
        .map_err(|err| rpc_error(4091, err.to_string()))?;

    Ok(json!({ "ok": true }))
}

async fn drain_ice(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_ref()
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
    let params: DrainIceParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for drain_ice: {err}")))?;

    let list = state
        .rooms
        .meeting_drain_ice(joined.room.clone(), &joined.participant_id, params.max)
        .map_err(|err| rpc_error(4092, err.to_string()))?;

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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
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
        .map_err(|err| rpc_error(4093, err.to_string()))?;
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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
    let params: TrackIdsParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for unpublish_tracks: {err}")))?;

    let revision = state
        .rooms
        .meeting_unpublish_tracks(joined.room.clone(), &joined.participant_id, params.track_ids)
        .map_err(|err| rpc_error(4094, err.to_string()))?;
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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
    let params: TrackIdsParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for subscribe: {err}")))?;

    let revision = state
        .rooms
        .meeting_subscribe(joined.room.clone(), &joined.participant_id, params.track_ids)
        .map_err(|err| rpc_error(4095, err.to_string()))?;
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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
    let params: TrackIdsParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for unsubscribe: {err}")))?;

    let revision = state
        .rooms
        .meeting_unsubscribe(joined.room.clone(), &joined.participant_id, params.track_ids)
        .map_err(|err| rpc_error(4096, err.to_string()))?;
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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;

    let list = state
        .rooms
        .meeting_list_publications(joined.room.clone())
        .map_err(|err| rpc_error(4097, err.to_string()))?;

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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;

    let list = state
        .rooms
        .meeting_list_streams(joined.room.clone())
        .map_err(|err| rpc_error(4102, err.to_string()))?;

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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;

    let list = state
        .rooms
        .meeting_list_participants(joined.room.clone())
        .map_err(|err| rpc_error(4099, err.to_string()))?;

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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;

    let requested_track_ids = state
        .rooms
        .meeting_list_subscription_requests(joined.room.clone(), &joined.participant_id)
        .map_err(|err| rpc_error(4098, err.to_string()))?;
    let effective_track_ids = state
        .rooms
        .meeting_list_effective_subscriptions(joined.room.clone(), &joined.participant_id)
        .map_err(|err| rpc_error(4098, err.to_string()))?;

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
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;

    let mode = state
        .rooms
        .meeting_policy_mode(joined.room.clone())
        .map_err(|err| rpc_error(4100, err.to_string()))?;

    Ok(json!({
        "mode": policy_mode_label(mode)
    }))
}

async fn set_policy_mode(state: &mut SessionState, params: Value) -> Result<Value, RpcError> {
    let joined = state
        .joined
        .as_mut()
        .ok_or_else(|| rpc_error(4003, "not joined".to_string()))?;
    let params: SetPolicyModeParams = serde_json::from_value(params)
        .map_err(|err| rpc_error(-32602, format!("invalid params for set_policy_mode: {err}")))?;

    let mode: MeetingPolicyMode = params.mode.into();
    let revision = state
        .rooms
        .meeting_set_policy_mode(joined.room.clone(), mode)
        .map_err(|err| rpc_error(4101, err.to_string()))?;
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
