mod ws;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};

use narwhal_core::{BroadcastPolicyMode, RoomId, RoomManager};

pub fn app(rooms: RoomManager) -> Router {
    Router::new()
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
        .route("/ws", get(ws::ws_upgrade))
        .with_state(rooms)
}

fn bad(msg: impl ToString) -> impl IntoResponse {
    (StatusCode::BAD_REQUEST, msg.to_string())
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
    State(rooms): State<RoomManager>,
    Path(room): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let offer_sdp = match std::str::from_utf8(&body) {
        Ok(s) => s.to_string(),
        Err(_) => return bad("invalid utf8 SDP").into_response(),
    };

    match rooms.whip_publish(RoomId(room), offer_sdp).await {
        Ok(res) => {
            let mut h = HeaderMap::new();
            h.insert("content-type", "application/sdp".parse().unwrap());
            h.insert("location", res.location.parse().unwrap());
            (StatusCode::CREATED, h, res.answer_sdp).into_response()
        }
        Err(e) => bad(e).into_response(),
    }
}

async fn whep_post(
    State(rooms): State<RoomManager>,
    Path(room): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let offer_sdp = match std::str::from_utf8(&body) {
        Ok(s) => s.to_string(),
        Err(_) => return bad("invalid utf8 SDP").into_response(),
    };

    match rooms.whep_subscribe(RoomId(room), offer_sdp).await {
        Ok(res) => {
            let mut h = HeaderMap::new();
            h.insert("content-type", "application/sdp".parse().unwrap());
            h.insert("location", res.location.parse().unwrap());
            (StatusCode::CREATED, h, res.answer_sdp).into_response()
        }
        Err(e) => bad(e).into_response(),
    }
}

async fn whip_patch(
    State(rooms): State<RoomManager>,
    Path((room, pub_id)): Path<(String, String)>,
    Json(payload): Json<IceIn>,
) -> impl IntoResponse {
    match rooms
        .whip_trickle(
            RoomId(room),
            &pub_id,
            payload.mline_index,
            payload.candidate,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bad(e).into_response(),
    }
}

async fn whep_patch(
    State(rooms): State<RoomManager>,
    Path((room, sub_id)): Path<(String, String)>,
    Json(payload): Json<IceIn>,
) -> impl IntoResponse {
    match rooms
        .whep_trickle(
            RoomId(room),
            &sub_id,
            payload.mline_index,
            payload.candidate,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bad(e).into_response(),
    }
}

async fn whip_get_ice(
    State(rooms): State<RoomManager>,
    Path((room, pub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match rooms.whip_drain_ice(RoomId(room), &pub_id, 50) {
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
        Err(e) => bad(e).into_response(),
    }
}

async fn whep_get_ice(
    State(rooms): State<RoomManager>,
    Path((room, sub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match rooms.whep_drain_ice(RoomId(room), &sub_id, 50) {
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
        Err(e) => bad(e).into_response(),
    }
}

async fn whip_delete(
    State(rooms): State<RoomManager>,
    Path((room, pub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match rooms.whip_stop(RoomId(room), &pub_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bad(e).into_response(),
    }
}

async fn whep_delete(
    State(rooms): State<RoomManager>,
    Path((room, sub_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match rooms.whep_stop(RoomId(room), &sub_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bad(e).into_response(),
    }
}

async fn get_broadcast_policy_mode(
    State(rooms): State<RoomManager>,
    Path(room): Path<String>,
) -> impl IntoResponse {
    match rooms.broadcast_policy_mode(RoomId(room)) {
        Ok(mode) => Json(BroadcastPolicyModeOut {
            mode: broadcast_policy_mode_label(mode),
        })
        .into_response(),
        Err(e) => bad(e).into_response(),
    }
}

async fn set_broadcast_policy_mode(
    State(rooms): State<RoomManager>,
    Path(room): Path<String>,
    Json(payload): Json<SetBroadcastPolicyModeIn>,
) -> impl IntoResponse {
    let mode: BroadcastPolicyMode = payload.mode.into();
    match rooms.broadcast_set_policy_mode(RoomId(room), mode) {
        Ok(()) => Json(BroadcastPolicyModeOut {
            mode: broadcast_policy_mode_label(mode),
        })
        .into_response(),
        Err(e) => bad(e).into_response(),
    }
}

async fn get_broadcast_inspection(
    State(rooms): State<RoomManager>,
    Path(room): Path<String>,
) -> impl IntoResponse {
    match rooms.broadcast_inspect(RoomId(room)) {
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
        Err(e) => bad(e).into_response(),
    }
}
