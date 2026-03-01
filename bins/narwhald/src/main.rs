use std::net::SocketAddr;

use anyhow::Result;
use axum::{
    Router,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
};
use media::GstRuntime;
use narwhal_core::RoomManager;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let gst = GstRuntime::init()?;
    let rooms = RoomManager::new(gst);

    let app = server::app(rooms).merge(clients_routes());

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    tracing::info!("listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn clients_routes() -> Router {
    Router::new()
        .route("/clients", get(clients_index))
        .route("/clients/", get(clients_index))
        .route("/clients/app.js", get(clients_app_js))
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
