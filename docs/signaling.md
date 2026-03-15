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

## Broadcast Signaling

Broadcast rooms use WHIP/WHEP-style signaling.

Room mode rules:

- broadcast endpoints are only valid for rooms in broadcast mode
- if a room is already active in meeting mode, broadcast endpoints return conflict-style errors

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

### Publisher ICE Trickle

Route:

- `PATCH /whip/:room/:publisher_id`

Request body:

```json
{
  "mline_index": 0,
  "candidate": "candidate:..."
}
```

Success response:

- `204 No Content`

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

### Subscriber ICE Trickle

Route:

- `PATCH /whep/:room/:subscriber_id`

Request body:

```json
{
  "mline_index": 0,
  "candidate": "candidate:..."
}
```

Success response:

- `204 No Content`

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
  "revision": 1
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
  "revision": 1
}
```

Current behavior:

- the participant must have joined first
- the server creates the underlying media peer lazily on first SDP offer
- revisions greater than the current room revision are rejected

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
  "needs_renegotiation": false
}
```

Current behavior:

- publishing increments room revision
- the API currently still reports `needs_renegotiation: false`

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
  "needs_renegotiation": false
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
  "needs_renegotiation": false
}
```

Notes:

- requested tracks are validated against currently known publications
- subscription policy may reduce the effective set relative to the requested set
- subscribing increments room revision
- the API currently still reports `needs_renegotiation: false`

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
  "needs_renegotiation": false
}
```

Current behavior:

- unsubscribing increments room revision
- the API currently still reports `needs_renegotiation: false`

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
  "needs_renegotiation": false
}
```

## Revision Semantics

Meeting rooms maintain a monotonically increasing room revision.

Current behavior:

- `join` returns the current revision after participant insertion
- mutating operations such as publish, unpublish, subscribe, unsubscribe, and policy changes increment the revision
- `sdp_offer` rejects revisions greater than the current room revision
- when a publisher leaves, their publications are removed and other participants' requested/effective subscriptions are pruned accordingly
- if that publisher rejoins later, they are a new session and remote participants must subscribe again to the newly published track IDs

Current limitation:

- the API already returns `revision`, but renegotiation is still not fully modeled
- many successful mutation responses currently return `"needs_renegotiation": false`
- clients should treat revision as authoritative room state versioning, but not as a finished renegotiation protocol

## Current Limitations

- there is no server-push event stream for room changes; clients poll via RPC methods
- ICE is drained by explicit client polling
- meeting renegotiation semantics are still incomplete
- broadcast subscribe currently requires the publisher to already be flowing RTP
- status codes and JSON-RPC error codes are stable enough for current tests, but the protocol should still be considered evolving
