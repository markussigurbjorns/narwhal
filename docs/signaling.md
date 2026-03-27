# Narwhal Signaling

This document describes the signaling contract implemented in the current codebase.

Narwhal currently exposes two signaling surfaces:

- broadcast signaling over HTTP with WHIP/WHEP-style routes
- meeting signaling over WebSocket using JSON-RPC 2.0 messages

It also exposes a small broadcast control surface over HTTP.

## Base URLs

Default server address:

- `http://localhost:8080`

WebSocket meeting endpoint:

- `ws://localhost:8080/ws`

When running behind TLS, use `https://` and `wss://` as appropriate.

## Error Model

HTTP endpoints use these status classes:

- `200 OK`
- `201 Created`
- `204 No Content`
- `503 Service Unavailable`
- `404 Not Found`
- `409 Conflict`
- `422 Unprocessable Entity`
- `500 Internal Server Error`

Error bodies use:

```json
{ "error": "message" }
```

Meeting JSON-RPC uses:

- standard JSON-RPC parse and method errors where applicable
- Narwhal transport-mapped application error codes:
  - `4040` not found
  - `4090` conflict
  - `4220` invalid request
  - `5000` internal error

Those codes are derived from the shared core error taxonomy.

Drain-mode admission behavior:

- during graceful shutdown drain, new session admission is rejected
- `POST /whip/:room`
- `POST /whep/:room`
- `GET /ws`
- these currently return `503` with:

```json
{ "error": "server is draining and not accepting new sessions" }
```

Existing sessions can still continue cleanup-oriented operations during drain, such as `DELETE`, ICE drain, and leave handling.

## Broadcast Signaling

Broadcast rooms use WHIP/WHEP-style signaling.

Room mode rules:

- broadcast endpoints are only valid for rooms in broadcast mode
- if a room is already active in meeting mode, broadcast endpoints return conflict-style errors

### Broadcast Session State

Broadcast room mode and media session state are currently simple:

| State | Meaning | Entered By | Exited By |
| --- | --- | --- | --- |
| empty broadcast room | room exists in broadcast mode, with no active publisher and no subscribers | `ensure_room_mode(..., Broadcast)` or a new broadcast room before publisher setup | first publisher attach or mode switch while still empty |
| active publisher | one WHIP publisher is registered for the room | successful `POST /whip/:room` | `DELETE /whip/:room/:publisher_id` or publisher replacement |
| active subscriber | one WHEP subscriber is registered for the room | successful `POST /whep/:room` | `DELETE /whep/:room/:subscriber_id`, slow-consumer eviction, or shutdown |

Current room-mode invariants:

- a non-empty broadcast room cannot be joined through meeting signaling
- a room already active in meeting mode rejects WHIP/WHEP broadcast signaling
- a brand-new publisher replaces the old publisher for that room
- subscribers are independent per resource and can be deleted idempotently

### Publish With WHIP

Route:

- `POST /whip/:room`

Request body:

- raw SDP offer
- content is expected to be UTF-8 text

Success response:

- `201 Created`
- `Content-Type: application/sdp`
- `Location: /whip/:room/:publisher_id`
- body is the SDP answer

Failure examples:

- `422` invalid UTF-8 SDP
- `409` room is in meeting mode
- `500` media/runtime failures

### Publisher PATCH

Route:

- `PATCH /whip/:room/:publisher_id`

JSON request body for ICE trickle:

```json
{
  "mline_index": 0,
  "candidate": "candidate:..."
}
```

Success response for ICE trickle:

- `204 No Content`

Raw SDP request body for renegotiation / ICE restart:

- `Content-Type: application/sdp`
- body is a new SDP offer for the existing publisher resource

Success response for SDP renegotiation:

- `200 OK`
- `Content-Type: application/sdp`
- body is the SDP answer

### Drain Server ICE For Publisher

Route:

- `GET /whip/:room/:publisher_id/ice`

Success response:

```json
[
  {
    "mline_index": 0,
    "candidate": "candidate:..."
  }
]
```

### Stop Publisher

Route:

- `DELETE /whip/:room/:publisher_id`

Success response:

- `204 No Content`

Notes:

- current behavior is replace-on-new-publisher
- starting a new publisher replaces the previous one for the room
- existing WHEP subscribers stay attached across publisher replacement
- when a publisher is replaced while subscribers are still present, the server immediately requests a fresh video keyframe from the new publisher to warm those subscribers back up
- deleting the same publisher resource more than once is treated as a no-op
- `PATCH` on an existing publisher resource now accepts either JSON ICE trickle or raw SDP renegotiation
- WHIP restart offers are accepted on the existing resource, but answer-side ICE credential rotation is still delegated to the underlying media stack and is not yet asserted as a stable wire guarantee

### Subscribe With WHEP

Route:

- `POST /whep/:room`

Request body:

- raw SDP offer

Success response:

- `201 Created`
- `Content-Type: application/sdp`
- `Location: /whep/:room/:subscriber_id`
- body is the SDP answer

Important current behavior:

- a publisher must already exist
- the publisher must already be flowing RTP
- otherwise subscription fails

Typical failure causes:

- `404` room not found
- `404` no publisher
- `422` publisher has not produced RTP yet
- `409` room is in meeting mode

### Subscriber PATCH

Route:

- `PATCH /whep/:room/:subscriber_id`

JSON request body for ICE trickle:

```json
{
  "mline_index": 0,
  "candidate": "candidate:..."
}
```

Success response for ICE trickle:

- `204 No Content`

Raw SDP request body for renegotiation / ICE restart:

- `Content-Type: application/sdp`
- body is a new SDP offer for the existing subscriber resource

Success response for SDP renegotiation:

- `200 OK`
- `Content-Type: application/sdp`
- body is the SDP answer

### Drain Server ICE For Subscriber

Route:

- `GET /whep/:room/:subscriber_id/ice`

Success response:

```json
[
  {
    "mline_index": 0,
    "candidate": "candidate:..."
  }
]
```

### Stop Subscriber

Route:

- `DELETE /whep/:room/:subscriber_id`

Success response:

- `204 No Content`

Notes:

- deleting the same subscriber resource more than once is treated as a no-op
- `PATCH` on an existing subscriber resource now accepts either JSON ICE trickle or raw SDP renegotiation
- WHEP restart offers are accepted on the existing resource, but answer-side ICE credential rotation is not yet documented as a stable guarantee

## Broadcast Control API

These routes are read/write control endpoints for broadcast rooms.

### Read Broadcast Policy

- `GET /control/rooms/:room/broadcast/policy`

Success response:

```json
{ "mode": "standard" }
```

Mode values:

- `standard`
- `low_bandwidth`
- `audio_only`

### Update Broadcast Policy

- `PUT /control/rooms/:room/broadcast/policy`

Request body:

```json
{ "mode": "audio_only" }
```

Accepted request mode values:

- `standard`
- `low_bandwidth`
- `audio_only`

Success response:

```json
{ "mode": "audio_only" }
```

### Inspect Broadcast State

- `GET /control/rooms/:room/broadcast`

Success response includes:

- current policy mode
- publisher id
- subscriber ids
- observed stream info
- per-subscriber plan details
- compiled forwarding graph edges

This endpoint is intended for debugging and inspection rather than end-user clients.

## Meeting Signaling

Meeting rooms use one WebSocket and JSON-RPC 2.0 messages.

Transport:

- connect to `/ws`
- send JSON-RPC request objects
- receive JSON-RPC response objects

Current model:

- one WebSocket session corresponds to at most one joined meeting participant
- the server assigns the participant ID
- if the WebSocket closes, the server treats that as participant leave
- reconnect is currently a fresh join, not session resume

Negotiation state model:

- each joined WebSocket session tracks its own negotiation state
- current values are:
  - `awaiting_initial_offer`
  - `stable`
  - `renegotiation_required`
- the initial `join` state is `awaiting_initial_offer`
- a successful `sdp_offer` moves the session to `stable`
- later session-affecting mutations can move the session to `renegotiation_required`
- once a session is already `renegotiation_required`, additional mutations keep it there until the next successful `sdp_offer`

### Meeting Session State

Meeting WebSocket session state is currently:

| State | Meaning | Entered By | Exited By |
| --- | --- | --- | --- |
| unjoined | websocket exists but has not joined a participant session | fresh `/ws` upgrade | successful `join` |
| awaiting_initial_offer | participant joined, but this websocket session has not completed SDP negotiation yet | successful `join` | successful `sdp_offer` or socket close/`leave` |
| stable | participant session has negotiated and is not currently dirty | successful `sdp_offer` | mutating action that changes session-visible state |
| renegotiation_required | participant session previously negotiated and then changed published/subscribed/policy state | publish, unpublish, subscribe, unsubscribe, or policy change after `stable` | next successful `sdp_offer` |

Current room-mode invariants:

- one websocket session can only join one participant at a time
- reconnect is modeled as a fresh participant session, not resume
- a room already active in broadcast mode rejects meeting signaling
- an empty room or empty broadcast room can be promoted into meeting mode on first successful `join`

### JSON-RPC Envelope

Request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "join",
  "params": { "room": "demo-room" }
}
```

Success response:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": { "...": "..." }
}
```

Error response:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": 4220,
    "message": "not joined"
  }
}
```

Notifications:

- if `id` is omitted, the server treats the message as a notification
- no response is sent

### Meeting Methods

#### `join`

Params:

```json
{
  "room": "demo-room",
  "display_name": "Alice"
}
```

Success result:

```json
{
  "participant_id": "uuid",
  "room": "demo-room",
  "mode": "meeting",
  "revision": 1,
  "negotiation_state": "awaiting_initial_offer"
}
```

Current behavior:

- if the room does not exist, it is created and switched to meeting mode
- if the room is already in broadcast mode, join fails
- if the WebSocket session already joined, join fails
- reconnecting after disconnect creates a new participant ID

#### `leave`

Params:

```json
{}
```

Success result:

```json
{ "left": true }
```

If the socket closes without an explicit `leave`, the server attempts cleanup automatically.

#### `sdp_offer`

Params:

```json
{
  "revision": 1,
  "offer_sdp": "v=0..."
}
```

Success result:

```json
{
  "answer_sdp": "v=0...",
  "revision": 1,
  "negotiation_state": "stable"
}
```

Current behavior:

- the participant must have joined first
- the server creates the underlying media peer lazily on first SDP offer
- revisions greater than the current room revision are rejected
- if the room revision advances while an SDP answer is being generated, the offer is rejected with a conflict-style error and the client must retry `sdp_offer` against the newer revision instead of applying a stale answer
- if the client sends a new offer with changed ICE credentials, the server treats that as an ICE restart and allows the answer to rotate its own ICE credentials instead of preserving the previous transport identity

#### `trickle_ice`

Params:

```json
{
  "mline_index": 0,
  "candidate": "candidate:..."
}
```

Success result:

```json
{ "ok": true }
```

#### `drain_ice`

Params:

```json
{
  "max": 50
}
```

`max` is optional and defaults to `50`.

Success result:

```json
{
  "candidates": [
    {
      "mline_index": 0,
      "candidate": "candidate:..."
    }
  ]
}
```

#### `publish_tracks`

Params:

```json
{
  "tracks": [
    {
      "track_id": "alice-audio",
      "media_kind": "audio",
      "mid": "0"
    }
  ]
}
```

Success result:

```json
{
  "revision": 2,
  "needs_renegotiation": false,
  "negotiation_state": "awaiting_initial_offer"
}
```

Current behavior:

- publishing increments room revision
- if this WebSocket session has not completed `sdp_offer` yet, `needs_renegotiation` remains `false` and `negotiation_state` remains `awaiting_initial_offer`
- after this WebSocket session has successfully negotiated once, publish changes return `needs_renegotiation: true` and `negotiation_state: "renegotiation_required"`
- repeated publish/unpublish/subscribe/unsubscribe/policy mutations while already dirty keep `negotiation_state` at `renegotiation_required`

#### `unpublish_tracks`

Params:

```json
{
  "track_ids": ["alice-audio"]
}
```

Success result:

```json
{
  "revision": 3,
  "needs_renegotiation": false,
  "negotiation_state": "awaiting_initial_offer"
}
```

#### `subscribe`

Params:

```json
{
  "track_ids": ["alice-audio", "alice-video"]
}
```

Success result:

```json
{
  "revision": 4,
  "needs_renegotiation": false,
  "negotiation_state": "awaiting_initial_offer"
}
```

Notes:

- requested tracks are validated against currently known publications
- subscription policy may reduce the effective set relative to the requested set
- subscribing increments room revision
- if this WebSocket session has not completed `sdp_offer` yet, `needs_renegotiation` remains `false` and `negotiation_state` remains `awaiting_initial_offer`
- after this WebSocket session has successfully negotiated once, subscribe changes return `needs_renegotiation: true` and `negotiation_state: "renegotiation_required"`

#### `unsubscribe`

Params:

```json
{
  "track_ids": ["alice-video"]
}
```

Success result:

```json
{
  "revision": 5,
  "needs_renegotiation": false,
  "negotiation_state": "awaiting_initial_offer"
}
```

Current behavior:

- unsubscribing increments room revision
- if this WebSocket session has not completed `sdp_offer` yet, `needs_renegotiation` remains `false` and `negotiation_state` remains `awaiting_initial_offer`
- after this WebSocket session has successfully negotiated once, unsubscribe changes return `needs_renegotiation: true` and `negotiation_state: "renegotiation_required"`

#### `list_participants`

Success result:

```json
{
  "participants": [
    {
      "participant_id": "uuid",
      "display_name": "Alice"
    }
  ]
}
```

#### `list_publications`

Success result:

```json
{
  "publications": [
    {
      "track_id": "alice-audio",
      "publisher_id": "uuid",
      "media_kind": "audio",
      "mid": "0"
    }
  ]
}
```

#### `list_streams`

Success result:

```json
{
  "streams": [
    {
      "track_id": "alice-video",
      "publisher_id": "uuid",
      "media_kind": "video",
      "ssrc": 1234,
      "encoding_id": "ssrc:1234",
      "mid": "1",
      "rid": "h",
      "spatial_layer": 0
    }
  ]
}
```

#### `list_subscriptions`

Success result:

```json
{
  "requested_track_ids": ["alice-video"],
  "effective_track_ids": ["alice-video"],
  "track_ids": ["alice-video"]
}
```

`track_ids` is currently an alias for `effective_track_ids`.

#### `get_policy_mode`

Success result:

```json
{ "mode": "standard" }
```

Accepted / returned values:

- `standard`
- `low_bandwidth`

#### `set_policy_mode`

Params:

```json
{ "mode": "low_bandwidth" }
```

Success result:

```json
{
  "mode": "low_bandwidth",
  "revision": 6,
  "needs_renegotiation": false,
  "negotiation_state": "awaiting_initial_offer"
}
```

Current behavior:

- if this WebSocket session has not completed `sdp_offer` yet, `needs_renegotiation` remains `false` and `negotiation_state` remains `awaiting_initial_offer`
- after this WebSocket session has successfully negotiated once, policy changes return `needs_renegotiation: true` and `negotiation_state: "renegotiation_required"`

## Revision Semantics

Meeting rooms maintain a monotonically increasing room revision.

Current behavior:

- `join` returns the current revision after participant insertion
- mutating operations such as publish, unpublish, subscribe, unsubscribe, and policy changes increment the revision
- `sdp_offer` rejects revisions greater than the current room revision
- `sdp_offer` also rejects answers superseded by a newer room revision observed before negotiation completes
- when a publisher leaves, their publications are removed and other participants' requested/effective subscriptions are pruned accordingly
- if that publisher rejoins later, they are a new session and remote participants must subscribe again to the newly published track IDs

Current limitation:

- the API now returns `revision`, `needs_renegotiation`, and a session-local `negotiation_state`, and also emits server-push `renegotiation_required` notifications for some room-wide changes
- clients should treat revision as authoritative room state versioning, but not as a finished renegotiation protocol for every participant in the room

## Current Limitations

- ICE is drained by explicit client polling
- meeting renegotiation semantics are more explicit now, but still incomplete at the room-wide orchestration level for broader browser recovery flows beyond the current ICE restart path
- broadcast subscribe currently requires the publisher to already be flowing RTP
- status codes and JSON-RPC error codes are stable enough for current tests, but the protocol should still be considered evolving
