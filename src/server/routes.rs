use super::CROSSCOPY_FILE_V3_HANDOFF_HEADER;
use super::handlers::{
    handle_cancel, handle_info, handle_prepare_upload, handle_register, handle_upload,
};
use super::state::ServerState;
use super::web_share::{
    handle_download, handle_prepare_download, handle_web_i18n, handle_web_index, handle_web_js,
};
use axum::{
    Router,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) fn create_router(state: Arc<RwLock<ServerState>>) -> Router {
    Router::new()
        .route("/api/localsend/v2/info", get(handle_info))
        .route("/api/localsend/v2/register", post(handle_register))
        .route(
            "/api/localsend/v2/prepare-upload",
            post(handle_prepare_upload),
        )
        .route("/api/localsend/v2/upload", post(handle_upload))
        .route("/api/localsend/v2/cancel", post(handle_cancel))
        .route(
            "/api/localsend/v2/prepare-download",
            post(handle_prepare_download),
        )
        .route("/api/localsend/v2/download", get(handle_download))
        .route("/", get(handle_web_index))
        .route("/main.js", get(handle_web_js))
        .route("/i18n.json", get(handle_web_i18n))
        // The reserved credential is meaningful only on one exact protected
        // prepare route.  Never let another LocalSend handler silently ignore
        // it and fall back into standard behavior.
        .layer(middleware::from_fn(reject_crosscopy_header_on_other_routes))
        .with_state(state)
}

async fn reject_crosscopy_header_on_other_routes(request: Request, next: Next) -> Response {
    if request
        .headers()
        .contains_key(CROSSCOPY_FILE_V3_HANDOFF_HEADER)
        && request.uri().path() != "/api/localsend/v2/prepare-upload"
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    next.run(request).await
}
