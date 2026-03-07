use std::collections::{HashMap, HashSet};

use crate::{ParticipantId, RoomId, TrackId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomMode {
    Broadcast,
    Meeting,
}

#[derive(Clone, Debug)]
pub struct MeetingRoomState {
    pub room_id: RoomId,
    pub participants: HashMap<ParticipantId, ParticipantState>,
    pub publications: HashMap<TrackId, Publication>,
    pub subscriptions: HashMap<ParticipantId, HashSet<TrackId>>,
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub struct ParticipantState {
    pub id: ParticipantId,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Publication {
    pub track_id: TrackId,
    pub publisher: ParticipantId,
    pub media_kind: MediaKind,
    pub mid: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

impl MeetingRoomState {
    pub fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
            participants: HashMap::new(),
            publications: HashMap::new(),
            subscriptions: HashMap::new(),
            revision: 0,
        }
    }

    pub fn next_revision(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
    }
}
