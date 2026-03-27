use std::{env, net::SocketAddr};

use anyhow::Result;
use axum::{
    Router,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
};
use media::GstRuntime;
use narwhal_core::{RoomManager, RoomManagerConfig};
use server::AppState;
use tokio::time::{Duration, sleep};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let gst = GstRuntime::init()?;
    let room_manager_config = room_manager_config()?;
    let shutdown_drain_secs = parse_env_u64("NARWHAL_SHUTDOWN_DRAIN_SECS", 10)?;
    let debug_snapshot_secs = parse_env_u64("NARWHAL_DEBUG_ROOM_SNAPSHOT_SECS", 0)?;
    tracing::info!(
        slow_subscriber_drop_streak_limit = room_manager_config.slow_subscriber_drop_streak_limit,
        shutdown_drain_secs,
        debug_room_snapshot_secs = debug_snapshot_secs,
        "loaded room manager config"
    );
    let rooms = RoomManager::with_config(gst, room_manager_config);
    let state = AppState::new(rooms);

    if debug_snapshot_secs > 0 {
        tokio::spawn(periodic_room_snapshot_logger(
            state.rooms(),
            Duration::from_secs(debug_snapshot_secs),
        ));
    }

    let app = server::app_with_state(state.clone()).merge(clients_routes());

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    tracing::info!("listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(
            state,
            Duration::from_secs(shutdown_drain_secs),
        ))
        .await?;

    Ok(())
}

fn room_manager_config() -> Result<RoomManagerConfig> {
    Ok(RoomManagerConfig {
        slow_subscriber_drop_streak_limit: parse_env_u32(
            "NARWHAL_SLOW_SUBSCRIBER_DROP_STREAK_LIMIT",
            RoomManagerConfig::default().slow_subscriber_drop_streak_limit,
        )?,
    })
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|err| anyhow::anyhow!("{name} must be a u32: {err}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(anyhow::anyhow!("failed to read {name}: {err}")),
    }
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|err| anyhow::anyhow!("{name} must be a u64: {err}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(anyhow::anyhow!("failed to read {name}: {err}")),
    }
}

async fn shutdown_signal(state: AppState, drain_duration: Duration) {
    wait_for_shutdown_signal().await;
    tracing::info!(
        drain_secs = drain_duration.as_secs(),
        "shutdown signal received; entering drain mode"
    );
    state.mark_draining();
    sleep(drain_duration).await;
    tracing::info!("drain window elapsed; stopping active peers");
    state.rooms().shutdown().await;
}

async fn periodic_room_snapshot_logger(rooms: RoomManager, interval: Duration) {
    loop {
        sleep(interval).await;
        let snapshots = rooms.debug_room_snapshots();
        tracing::info!(
            room_count = snapshots.len(),
            "periodic room snapshot"
        );
        for snapshot in snapshots {
            tracing::info!(
                room = %snapshot.room_id,
                mode = ?snapshot.mode,
                broadcast_policy_mode = ?snapshot.broadcast_policy_mode,
                meeting_policy_mode = ?snapshot.meeting_policy_mode,
                meeting_revision = snapshot.meeting_revision,
                broadcast_publisher_active = snapshot.broadcast_publisher_active,
                broadcast_subscribers = snapshot.broadcast_subscribers,
                broadcast_streams = snapshot.broadcast_streams,
                meeting_participants = snapshot.meeting_participants,
                meeting_publications = snapshot.meeting_publications,
                meeting_streams = snapshot.meeting_streams,
                "room snapshot"
            );
        }
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    }
}

fn clients_routes() -> Router {
    Router::new()
        .route("/clients", get(clients_index))
        .route("/clients/", get(clients_index))
        .route("/clients/app.js", get(clients_app_js))
        .route("/clients/broadcast", get(clients_broadcast))
        .route("/clients/broadcast/", get(clients_broadcast))
        .route("/clients/broadcast.js", get(clients_broadcast_js))
        .route("/clients/meeting", get(clients_meeting))
        .route("/clients/meeting/", get(clients_meeting))
        .route("/clients/meeting.js", get(clients_meeting_js))
}

async fn clients_index() -> Html<&'static str> {
    Html(include_str!("../../../clients/index.html"))
}

async fn clients_app_js() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );

    (
        StatusCode::OK,
        headers,
        include_str!("../../../clients/app.js"),
    )
}

async fn clients_broadcast() -> Html<&'static str> {
    Html(include_str!("../../../clients/broadcast.html"))
}

async fn clients_broadcast_js() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );

    (
        StatusCode::OK,
        headers,
        include_str!("../../../clients/broadcast.js"),
    )
}

async fn clients_meeting() -> Html<&'static str> {
    Html(include_str!("../../../clients/meeting.html"))
}

async fn clients_meeting_js() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );

    (
        StatusCode::OK,
        headers,
        include_str!("../../../clients/meeting.js"),
    )
}
