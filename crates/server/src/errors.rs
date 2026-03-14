use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use narwhal_core::{Error as CoreError, ErrorCategory};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    Conflict,
    InvalidRequest,
    Internal,
}

#[derive(Debug)]
pub struct TransportError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl TransportError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, message)
    }

    pub fn from_anyhow(err: anyhow::Error) -> Self {
        if let Some(core) = err.downcast_ref::<CoreError>() {
            return Self {
                kind: match core.category() {
                    ErrorCategory::NotFound => ErrorKind::NotFound,
                    ErrorCategory::Conflict => ErrorKind::Conflict,
                    ErrorCategory::InvalidRequest => ErrorKind::InvalidRequest,
                    ErrorCategory::Internal => ErrorKind::Internal,
                },
                message: core.to_string(),
            };
        }

        Self::new(ErrorKind::Internal, err.to_string())
    }

    pub fn cause_label_from_anyhow(err: &anyhow::Error) -> &'static str {
        err.downcast_ref::<CoreError>()
            .map(CoreError::cause_label)
            .unwrap_or("internal")
    }

    pub fn status_code(&self) -> StatusCode {
        match self.kind {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::InvalidRequest => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn rpc_code(&self) -> i32 {
        match self.kind {
            ErrorKind::NotFound => 4040,
            ErrorKind::Conflict => 4090,
            ErrorKind::InvalidRequest => 4220,
            ErrorKind::Internal => 5000,
        }
    }
}

impl IntoResponse for TransportError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = axum::Json(ErrorBody {
            error: &self.message,
        });
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorKind, TransportError};
    use narwhal_core::Error as CoreError;

    #[test]
    fn classifies_not_found_errors_from_core_error() {
        let err = TransportError::from_anyhow(CoreError::RoomNotFound.into());
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    #[test]
    fn classifies_conflict_errors_from_core_error() {
        let err = TransportError::from_anyhow(
            CoreError::BroadcastEndpointsUnavailableInMeetingMode.into(),
        );
        assert_eq!(err.kind, ErrorKind::Conflict);
    }

    #[test]
    fn classifies_invalid_request_errors_from_core_error() {
        let err = TransportError::from_anyhow(CoreError::UnknownTrackRequested.into());
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn exposes_cause_labels_from_core_errors() {
        let err: anyhow::Error = CoreError::AlreadyJoined.into();
        assert_eq!(
            TransportError::cause_label_from_anyhow(&err),
            "already_joined"
        );
    }
}
