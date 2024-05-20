use crate::{cloud, AuthData};
use arroyo_server_common::log_event;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, TypedHeader};
use serde_json::json;
use tracing::{error, warn};

use axum::headers::authorization::{Authorization, Bearer};
use cornucopia_async::{DatabaseSource, DbError};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub type BearerAuth = Option<TypedHeader<Authorization<Bearer>>>;

const DEFAULT_ITEMS_PER_PAGE: u32 = 10;

#[derive(Debug, ToSchema, Serialize, Deserialize)]
pub struct ErrorResp {
    #[serde(skip)]
    pub(crate) status_code: StatusCode,
    #[serde(rename = "error")]
    pub(crate) message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    JsonExtractorRejection(#[from] JsonRejection),
}

pub fn map_insert_err(name: &str, error: DbError) -> ErrorResp {
    if error == DbError::DuplicateViolation {
        return bad_request(format!("{} with that name already exists", name));
    } else {
        error.into()
    }
}

pub fn map_delete_err(name: &str, user: &str, error: DbError) -> ErrorResp {
    if error == DbError::ForeignKeyViolation {
        return bad_request(format!(
            "Cannot delete {}; it is still being used by {}",
            name, user
        ));
    } else {
        error.into()
    }
}

impl From<DbError> for ErrorResp {
    fn from(value: DbError) -> Self {
        match value {
            DbError::DuplicateViolation => bad_request("A record already exists with that name"),
            DbError::ForeignKeyViolation => {
                bad_request("Cannot delete; other records depend on this one")
            }
            DbError::Other(e) => {
                warn!("Unhandled database error {}", e);
                ErrorResp {
                    status_code: StatusCode::INTERNAL_SERVER_ERROR,
                    message: e,
                }
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::JsonExtractorRejection(json_rejection) => {
                (json_rejection.status(), json_rejection.body_text())
            }
        };

        ErrorResp {
            status_code: status,
            message,
        }
        .into_response()
    }
}

pub fn log_and_map<E>(err: E) -> ErrorResp
where
    E: core::fmt::Debug,
{
    error!("Error while handling: {:?}", err);
    log_event("api_error", json!({ "error": format!("{:?}", err) }));
    ErrorResp {
        status_code: StatusCode::INTERNAL_SERVER_ERROR,
        message: "Something went wrong".to_string(),
    }
}

impl IntoResponse for ErrorResp {
    fn into_response(self) -> Response {
        let status_code = self.status_code;
        let body = Json(serde_json::to_value(self).unwrap());
        (status_code, body).into_response()
    }
}

pub(crate) async fn authenticate(
    db: &DatabaseSource,
    bearer_auth: BearerAuth,
) -> Result<AuthData, ErrorResp> {
    cloud::authenticate(&db.client().await?, bearer_auth).await
}

pub(crate) fn bad_request(message: impl Into<String>) -> ErrorResp {
    ErrorResp {
        status_code: StatusCode::BAD_REQUEST,
        message: message.into(),
    }
}

pub(crate) fn service_unavailable(object: &str) -> ErrorResp {
    ErrorResp {
        status_code: StatusCode::SERVICE_UNAVAILABLE,
        message: format!("{} not available", object),
    }
}

pub(crate) fn internal_server_error(message: impl Into<String>) -> ErrorResp {
    ErrorResp {
        status_code: StatusCode::INTERNAL_SERVER_ERROR,
        message: message.into(),
    }
}

pub(crate) fn not_found(object: &str) -> ErrorResp {
    ErrorResp {
        status_code: StatusCode::NOT_FOUND,
        message: format!("{} not found", object),
    }
}

pub(crate) fn required_field(field: &str) -> ErrorResp {
    bad_request(format!("Field {} must be set", field))
}

pub fn validate_pagination_params(
    starting_after: Option<String>,
    limit: Option<u32>,
) -> Result<(Option<String>, u32), ErrorResp> {
    // return ErrorResp if limit is less than 1
    if let Some(limit) = limit {
        if limit < 1 {
            return Err(ErrorResp {
                status_code: StatusCode::BAD_REQUEST,
                message: "Limit must be greater than 0".to_string(),
            });
        }
    }

    // increase limit by 1 to determine if there are more results
    let limit = limit.unwrap_or(DEFAULT_ITEMS_PER_PAGE) + 1;

    Ok((starting_after.clone(), limit))
}

pub fn paginate_results<T>(results: Vec<T>, limit: u32) -> (Vec<T>, bool) {
    // this limit is one more than the requested limit to determine if there are more results
    let mut results = results;
    let has_more = results.len() as u32 == limit;
    if has_more {
        results.pop();
    }

    (results, has_more)
}
