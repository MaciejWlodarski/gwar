//! Public ratchet trees for authorized conversation readers.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::gateway::Gateway;

pub async fn tree(State(gateway): State<Gateway>, Path(group): Path<String>, headers: HeaderMap) -> Response {
    let Some(access) =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((epoch, tree)) = gateway.core.mls_tree(group, access.to_owned()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE.as_str(), "application/octet-stream".to_owned()),
            (header::CACHE_CONTROL.as_str(), "no-store".to_owned()),
            ("x-gwar-epoch", epoch.to_string()),
        ],
        tree,
    )
        .into_response()
}
