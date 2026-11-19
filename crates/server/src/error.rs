use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub itm_code: Option<i32>,
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            itm_code: None,
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            itm_code: None,
        }
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
            itm_code: None,
        }
    }
}

impl From<itm::ItmError> for AppError {
    fn from(e: itm::ItmError) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: e.message().into(),
            itm_code: Some(e.code()),
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        if msg.contains("zone trop grande") {
            return Self::bad_request(msg);
        }
        tracing::error!(error = %msg, "erreur interne");
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("accès MNE : {msg}"),
            itm_code: None,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.message, "itm_code": self.itm_code });
        (self.status, Json(body)).into_response()
    }
}
