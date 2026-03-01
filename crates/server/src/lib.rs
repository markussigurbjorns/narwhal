use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};

use narwhal_core::{RoomId, RoomManager};

pub fn app(rooms: RoomManager) -> Router {
    Router::new()
        .route("/whip/{room}", post(whip_post))
        .route("/whip/{room}/{pub}/ice", get(whip_get_ice))
        .route("/whip/{room}/{pub}", delete(whip_delete).patch(whip_patch))
        .route("/whep/{room}", post(whep_post))
        .route("/whep/{room}/{sub}/ice", get(whep_get_ice))
        .route("/whep/{room}/{sub}", delete(whep_delete).patch(whep_patch))
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
