# Narwhal Observability

Narwhal exposes Prometheus-compatible metrics at:

- `GET /metrics`

The response content type is:

- `text/plain; version=0.0.4; charset=utf-8`

This document defines the current metric families and intended label meanings.

## Conventions

- Metrics are process-local. In the current architecture, one room is expected to live on one node.
- Room and peer counts are gauges.
- Lifecycle and failure counts are counters.
- Startup and first-media timings are histograms in seconds.
- Labels are intentionally low-cardinality. There are no room IDs or participant IDs in metrics.

## Room State

- `narwhal_rooms_total`
  - Gauge. Total rooms known by the process.
- `narwhal_rooms{mode}`
  - Gauge. Rooms by active mode.
  - `mode` values:
    - `broadcast`
    - `meeting`

## Live Session State

- `narwhal_broadcast_publishers_active`
  - Gauge. Active broadcast publishers.
- `narwhal_broadcast_subscribers_active`
  - Gauge. Active broadcast subscribers.
- `narwhal_meeting_participants_active`
  - Gauge. Active meeting participants.
- `narwhal_meeting_publications_active`
  - Gauge. Active meeting publications.

## Meeting Lifecycle

- `narwhal_meeting_joins_total`
- `narwhal_meeting_leaves_total`
- `narwhal_meeting_publish_tracks_total`
- `narwhal_meeting_unpublish_tracks_total`
- `narwhal_meeting_subscribe_requests_total`
- `narwhal_meeting_unsubscribe_requests_total`
- `narwhal_meeting_sdp_offers_total`
- `narwhal_meeting_trickle_ice_total`

All of the above are counters.

## Broadcast Lifecycle

- `narwhal_broadcast_publishers_started_total`
- `narwhal_broadcast_publishers_stopped_total`
- `narwhal_broadcast_subscribers_started_total`
- `narwhal_broadcast_subscribers_stopped_total`
- `narwhal_broadcast_whip_trickle_ice_total`
- `narwhal_broadcast_whep_trickle_ice_total`

All of the above are counters.

## Negotiation Metrics

- `narwhal_negotiation_total{flow,outcome,cause}`
  - Counter. Negotiation attempts by flow and result.

`flow` values currently include:

- `meeting_sdp_offer`
- `broadcast_whip_publish`
- `broadcast_whep_subscribe`

`outcome` values:

- `success`
- `failure`

`cause` values:

- `none`
  - Used for successful attempts.
- `room_not_found`
- `participant_not_joined`
- `meeting_participant_session_not_found`
- `meeting_publisher_session_not_found`
- `subscriber_not_found`
- `no_publisher`
- `publisher_id_mismatch`
- `already_joined`
- `not_joined`
- `unknown_track_requested`
- `track_ownership_conflict`
- `room_already_active_in_broadcast_mode`
- `room_mode_conflict`
- `stale_or_future_revision`
- `publisher_not_flowing`
- `injector_missing`
- `internal`

These values come from the shared core error taxonomy in `narwhal-core`.

## Peer State Metrics

- `narwhal_peer_state_transitions_total{role,state}`
  - Counter. Peer state transitions emitted by the media layer.

`role` values:

- `broadcast_publisher`
- `broadcast_subscriber`
- `meeting_participant`

`state` values:

- `new`
- `negotiating`
- `connected`
- `failed`
- `closed`

These are transition counters, not live-state gauges.

## RTP Drop Metrics

- `narwhal_meeting_rtp_dropped_total`
  - Counter. Aggregate meeting RTP drops.
- `narwhal_broadcast_rtp_dropped_total`
  - Counter. Aggregate broadcast RTP drops.
- `narwhal_rtp_dropped_total{mode,reason}`
  - Counter. RTP packets not forwarded, with labels.

`mode` values:

- `meeting`
- `broadcast`

`reason` values currently include:

- `queue_full`
- `channel_closed`

## Overload And Eviction

Narwhal forwards RTP by pushing cloned packets into a bounded per-subscriber injector queue.

That means overload in the current system does not mean "the whole process is out of CPU". It means a specific downstream subscriber or meeting participant is not draining forwarded RTP fast enough. When that happens:

1. `try_send` to that subscriber's injector queue starts failing with `queue_full`
2. the packet is dropped for that subscriber only
3. a consecutive overflow streak is tracked for that subscriber
4. once the streak reaches the configured threshold, that subscriber is evicted and its peer is stopped

This is a containment mechanism. The goal is to stop one persistently slow consumer from accumulating unbounded queue pressure or stalling forwarding work for the rest of the room.

Current eviction policy:

- threshold is controlled by `NARWHAL_SLOW_SUBSCRIBER_DROP_STREAK_LIMIT`
- default is `64`
- successful packet delivery resets the overflow streak
- a closed injector channel causes immediate eviction

Metrics for this behavior:

- `narwhal_rtp_dropped_total{mode,reason}`
  - `reason="queue_full"` means packets were dropped because the subscriber queue was already full
  - `reason="channel_closed"` means forwarding attempted to a closed injector
- `narwhal_subscriber_evictions_total{mode,reason}`
  - Counter. Subscribers or meeting participants removed from forwarding because delivery could not be sustained

`narwhal_subscriber_evictions_total{mode,reason}` values currently include:

- `mode="meeting"` or `mode="broadcast"`
- `reason="slow_consumer"`
- `reason="channel_closed"`

Operational interpretation:

- rising `queue_full` with no evictions means subscribers are briefly falling behind but recovering
- rising `queue_full` followed by `slow_consumer` evictions means the threshold is being hit and users are being disconnected
- `channel_closed` evictions usually indicate teardown races or stale forwarding state rather than bandwidth pressure

## First-Media Histograms

All timing histograms are measured in seconds.

Broadcast:

- `narwhal_broadcast_subscribe_to_first_rtp_seconds`
  - Time from subscriber creation to first forwarded RTP packet.
- `narwhal_broadcast_subscribe_to_first_video_keyframe_seconds`
  - Time from subscriber creation to first forwarded video keyframe.

Meeting:

- `narwhal_meeting_join_to_first_rtp_seconds`
  - Time from meeting join to first forwarded RTP packet.
- `narwhal_meeting_join_to_first_video_keyframe_seconds`
  - Time from meeting join to first forwarded video keyframe.
- `narwhal_meeting_subscribe_to_first_rtp_seconds`
  - Time from meeting subscribe request to first forwarded RTP packet.
- `narwhal_meeting_subscribe_to_first_video_keyframe_seconds`
  - Time from meeting subscribe request to first forwarded video keyframe.

## Logging Alignment

Warning logs for negotiation and room/media failures should carry the same stable `cause` label used in:

- `narwhal_negotiation_total{...,cause}`
- `TransportError::cause_label_from_anyhow(...)`
- `narwhal-core::Error::cause_label()`

This keeps metrics, HTTP/JSON-RPC transport behavior, and tracing output aligned on one taxonomy.

## Current Gaps

The current metrics do not yet include:

- per-phase negotiation timing
- ICE connection-state duration histograms
- bitrate or packet-rate gauges
- RTP ingress/egress counters
- per-room capacity or admission metrics
- active speaker or subscription distribution metrics

Those should be added only if they remain low-cardinality and operationally actionable.
