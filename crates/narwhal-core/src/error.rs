#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCategory {
    NotFound,
    Conflict,
    InvalidRequest,
    Internal,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("room not found")]
    RoomNotFound,
    #[error("participant not joined")]
    ParticipantNotJoined,
    #[error("meeting participant session not found")]
    MeetingParticipantSessionNotFound,
    #[error("meeting publisher session not found")]
    MeetingPublisherSessionNotFound,
    #[error("subscriber not found")]
    SubscriberNotFound,
    #[error("no publisher")]
    NoPublisher,
    #[error("publisher id mismatch")]
    PublisherIdMismatch,
    #[error("already joined; leave first or reconnect")]
    AlreadyJoined,
    #[error("not joined")]
    NotJoined,
    #[error("unknown track requested")]
    UnknownTrackRequested,
    #[error("cannot unpublish track not owned by participant")]
    CannotUnpublishTrackNotOwned,
    #[error("room already active in broadcast mode and cannot switch to meeting mode")]
    RoomAlreadyActiveInBroadcastMode,
    #[error("room is in meeting mode; WHIP/WHEP broadcast endpoints are not available")]
    BroadcastEndpointsUnavailableInMeetingMode,
    #[error("room is in broadcast mode; meeting websocket signaling is not available")]
    MeetingSignalingUnavailableInBroadcastMode,
    #[error("invalid future revision: got {got}, current {current}")]
    InvalidFutureRevision { got: u64, current: u64 },
    #[error("publisher has not produced RTP yet; try subscribing after media is flowing")]
    PublisherNotFlowing,
    #[error("whep injector was not created during negotiation")]
    WhepInjectorNotCreated,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl Error {
    pub fn category(&self) -> ErrorCategory {
        match self {
            Error::RoomNotFound
            | Error::ParticipantNotJoined
            | Error::MeetingParticipantSessionNotFound
            | Error::MeetingPublisherSessionNotFound
            | Error::SubscriberNotFound
            | Error::NoPublisher => ErrorCategory::NotFound,
            Error::PublisherIdMismatch
            | Error::RoomAlreadyActiveInBroadcastMode
            | Error::BroadcastEndpointsUnavailableInMeetingMode
            | Error::MeetingSignalingUnavailableInBroadcastMode => ErrorCategory::Conflict,
            Error::NotJoined
            | Error::UnknownTrackRequested
            | Error::CannotUnpublishTrackNotOwned
            | Error::InvalidFutureRevision { .. }
            | Error::PublisherNotFlowing
            | Error::WhepInjectorNotCreated => ErrorCategory::InvalidRequest,
            Error::AlreadyJoined => ErrorCategory::Conflict,
            Error::Internal(_) => ErrorCategory::Internal,
        }
    }

    pub fn cause_label(&self) -> &'static str {
        match self {
            Error::RoomNotFound => "room_not_found",
            Error::ParticipantNotJoined => "participant_not_joined",
            Error::MeetingParticipantSessionNotFound => "meeting_participant_session_not_found",
            Error::MeetingPublisherSessionNotFound => "meeting_publisher_session_not_found",
            Error::SubscriberNotFound => "subscriber_not_found",
            Error::NoPublisher => "no_publisher",
            Error::PublisherIdMismatch => "publisher_id_mismatch",
            Error::AlreadyJoined => "already_joined",
            Error::NotJoined => "not_joined",
            Error::UnknownTrackRequested => "unknown_track_requested",
            Error::CannotUnpublishTrackNotOwned => "track_ownership_conflict",
            Error::RoomAlreadyActiveInBroadcastMode => "room_already_active_in_broadcast_mode",
            Error::BroadcastEndpointsUnavailableInMeetingMode
            | Error::MeetingSignalingUnavailableInBroadcastMode => "room_mode_conflict",
            Error::InvalidFutureRevision { .. } => "stale_or_future_revision",
            Error::PublisherNotFlowing => "publisher_not_flowing",
            Error::WhepInjectorNotCreated => "injector_missing",
            Error::Internal(_) => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorCategory};

    #[test]
    fn categories_match_transport_expectations() {
        assert_eq!(Error::RoomNotFound.category(), ErrorCategory::NotFound);
        assert_eq!(Error::AlreadyJoined.category(), ErrorCategory::Conflict);
        assert_eq!(Error::NotJoined.category(), ErrorCategory::InvalidRequest);
        assert_eq!(
            Error::InvalidFutureRevision { got: 3, current: 2 }.category(),
            ErrorCategory::InvalidRequest
        );
    }

    #[test]
    fn cause_labels_are_stable() {
        assert_eq!(Error::RoomNotFound.cause_label(), "room_not_found");
        assert_eq!(Error::AlreadyJoined.cause_label(), "already_joined");
        assert_eq!(Error::NotJoined.cause_label(), "not_joined");
        assert_eq!(
            Error::MeetingSignalingUnavailableInBroadcastMode.cause_label(),
            "room_mode_conflict"
        );
    }
}
