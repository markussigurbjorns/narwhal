# Narwhal Configuration

Narwhal does not have a full config system yet. Runtime settings are currently a small set of environment variables read by the binary and the media layer.

This document defines the settings that affect current server behavior.

## Application

- `NARWHAL_SLOW_SUBSCRIBER_DROP_STREAK_LIMIT`
  - Type: `u32`
  - Default: `64`
  - Controls how many consecutive RTP forwarding overflows a single broadcast subscriber or meeting participant can accumulate before being evicted.

- `NARWHAL_SHUTDOWN_DRAIN_SECS`
  - Type: `u64`
  - Default: `10`
  - Controls how long `narwhald` stays in drain mode after receiving shutdown before active peers are stopped.

What it means:

- forwarded RTP is written into a bounded per-subscriber injector queue
- if that queue is full, the packet is dropped for that subscriber and the overflow streak increases
- any successful packet delivery resets the streak to `0`
- once the streak reaches this threshold, the subscriber is removed from room forwarding state and its peer is stopped

Tradeoff:

- lower values disconnect slow consumers faster and protect the room more aggressively
- higher values tolerate longer bursts of downstream slowness, but allow more repeated packet loss before the server gives up on that subscriber

This is a per-subscriber backpressure threshold, not a global room capacity setting.

At startup, `narwhald` logs the effective value it loaded for this threshold. That log line is the current source of truth for what policy a node is actually running with.
It also logs the effective shutdown drain window.

## Health Endpoints

Narwhal currently exposes:

- `GET /healthz`
  - returns `200` with `{ "ok": true }`
- `GET /readyz`
  - returns `200` with `{ "ready": true }`

Current meaning:

- `healthz` is a basic liveness check for the HTTP process
- `readyz` now means:
  - the process is not in drain mode
  - the media layer can create, start, and stop a probe peer successfully
- readiness probe results are cached briefly inside the server so repeated load-balancer polls do not create a fresh probe peer on every request
- the current cache TTL is `1` second
- during graceful shutdown drain, `readyz` returns `{ "ready": false }` so upstream load balancers can stop sending new traffic before peers are torn down

Current limitation:

- `readyz` is deeper than pure HTTP liveness, but it is still not a full end-to-end media readiness signal
- it does not validate TURN reachability, remote ICE success, or downstream network dependencies

## Media / ICE

The media layer also reads environment variables directly for WebRTC ICE configuration. Those are applied in `crates/media`.

- `NARWHAL_STUN_SERVER`
  - Example: `stun://stun.example.com:3478`
  - Sets `webrtcbin`'s `stun-server` property.

- `NARWHAL_TURN_SERVER`
  - Example: `turn://user:pass@turn.example.com:3478?transport=udp`
  - Sets `webrtcbin`'s `turn-server` property.

- `NARWHAL_ICE_TRANSPORT_POLICY`
  - Allowed values: `all`, `relay`
  - Default: unset, which leaves the media layer on the underlying `webrtcbin` default
  - `relay` forces relay-only ICE gathering/selection, which is the main knob needed for TURN-only deployment and browser interop testing.

If you extend runtime settings further, prefer moving them into typed config structs at the crate boundary instead of scattering new `env::var` calls through room logic.
