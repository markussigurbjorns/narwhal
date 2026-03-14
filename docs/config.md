# Narwhal Configuration

Narwhal does not have a full config system yet. Runtime settings are currently a small set of environment variables read by the binary and the media layer.

This document defines the settings that affect current server behavior.

## Application

- `NARWHAL_SLOW_SUBSCRIBER_DROP_STREAK_LIMIT`
  - Type: `u32`
  - Default: `64`
  - Controls how many consecutive RTP forwarding overflows a single broadcast subscriber or meeting participant can accumulate before being evicted.

What it means:

- forwarded RTP is written into a bounded per-subscriber injector queue
- if that queue is full, the packet is dropped for that subscriber and the overflow streak increases
- any successful packet delivery resets the streak to `0`
- once the streak reaches this threshold, the subscriber is removed from room forwarding state and its peer is stopped

Tradeoff:

- lower values disconnect slow consumers faster and protect the room more aggressively
- higher values tolerate longer bursts of downstream slowness, but allow more repeated packet loss before the server gives up on that subscriber

This is a per-subscriber backpressure threshold, not a global room capacity setting.

## Media / ICE

The media layer also reads environment variables directly for WebRTC ICE configuration. Those are applied in `crates/media`.

If you extend runtime settings further, prefer moving them into typed config structs at the crate boundary instead of scattering new `env::var` calls through room logic.
