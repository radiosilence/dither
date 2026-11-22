//! Errors log their cause in full and tell the client almost nothing: an
//! upstream or storage message quoted back to the browser can describe what
//! is behind it.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Internal(e.into())
    }
}

impl From<object_store::Error> for AppError {
    fn from(e: object_store::Error) -> Self {
        AppError::Internal(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, "request rejected");
        }
        // Only a rejection the caller can act on is repeated back.
        let public = match &self {
            AppError::BadRequest(m) => m.clone(),
            AppError::Unauthorized => "unauthorized".into(),
            AppError::NotFound => "not found".into(),
            AppError::Upstream(_) => "upstream error".into(),
            AppError::Internal(_) => "internal error".into(),
        };
        (status, public).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
