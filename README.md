# Narwhal

Narwhal is a Rust WebRTC SFU in progress.

It currently supports two room modes:

- broadcast: one publisher to many subscribers over WHIP/WHEP-style HTTP signaling
- meeting: many participants with selective forwarding over WebSocket JSON-RPC signaling

The current v1 media target is:

- audio: Opus
- video: VP8
- simulcast: optional, but supported

## Workspace

- `bins/narwhald`
  - binary entry point
- `crates/media`
  - GStreamer / WebRTC peer and RTP plumbing
- `crates/narwhal-core`
  - room state, policies, forwarding, metrics, shared errors
- `crates/server`
  - Axum routes for WHIP, WHEP, control, metrics, and meeting WebSocket signaling

## Run

Start the server with:

```bash
cargo run -p narwhald
```

The server listens on:

- `http://0.0.0.0:8080`

Browser demos are served at:

- `/clients`
- `/clients/broadcast`
- `/clients/meeting`

Metrics are exposed at:

- `/metrics`

## Development Notes

Project scope:

- [docs/v1-scope.md](/home/maggi/Documents/workspace/narwhal/docs/v1-scope.md)

Observability contract:

- [docs/observability.md](/home/maggi/Documents/workspace/narwhal/docs/observability.md)

Runtime configuration:

- [docs/config.md](/home/maggi/Documents/workspace/narwhal/docs/config.md)

Signaling contract:

- [docs/signaling.md](/home/maggi/Documents/workspace/narwhal/docs/signaling.md)

Architecture sketch:

- [docs/architecture.dot](/home/maggi/Documents/workspace/narwhal/docs/architecture.dot)

## Current Status

The repo is beyond prototype scaffolding, but it is not yet a finished production SFU.

Implemented areas include:

- broadcast and meeting room modes
- shared error taxonomy across core, transport, metrics, and logs
- Prometheus metrics and first-media timing histograms
- bounded-queue overload handling with slow-subscriber eviction
- browser demo clients
- unit and route-level tests for the current signaling and observability layers

Still in progress:

- broader signaling and media interoperability coverage
- overload handling and production tuning
- deployment guidance and operations polish
- fuller end-to-end integration coverage
