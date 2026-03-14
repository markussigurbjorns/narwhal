# Narwhal V1 Scope

Narwhal v1 targets two room modes:

- broadcast: one publisher to many subscribers over WHIP/WHEP-style signaling
- meeting: many participants with selective forwarding over WebSocket JSON-RPC signaling

Media scope for v1:

- audio codec: Opus
- video codec: VP8
- simulcast: optional, but supported when the browser/client offers it

Non-goals for the first production milestone:

- transcoding
- server-side mixing or compositing
- multi-codec optimization beyond Opus and VP8
- multi-node cascaded forwarding

Operational assumptions for the first production milestone:

- one room is pinned to one media node
- TURN support is expected for restrictive networks
- meeting and broadcast mode are both supported, but with explicit room-mode separation

Primary engineering priorities:

1. signaling and room lifecycle correctness
2. observability and overload handling
3. negotiation reliability, especially for optional simulcast
4. security, deployment hardening, and load-tested capacity limits
