use super::CROSSCOPY_FILE_V3_HANDOFF_HEADER;
use super::handlers::{
    handle_cancel, handle_info, handle_prepare_upload, handle_register, handle_upload,
};
use super::state::ServerState;
use super::web_share::{WebShareHost, web_share_router};
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

pub(crate) fn create_router(
    state: Arc<RwLock<ServerState>>,
    // Passed rather than read out of `state`: this is called from an async
    // context, where taking that lock to build a router would be a deadlock
    // waiting for a reason.
    web: Arc<RwLock<WebShareHost>>,
) -> Router {
    // The Web Share half is mounted from the same function every other consumer
    // uses, over its own state, so this crate's router and an embedder's cannot
    // drift apart. See `web_share::WebShareHost`.
    let web = web_share_router(web);
    Router::new()
        .route("/api/localsend/v2/info", get(handle_info))
        .route("/api/localsend/v2/register", post(handle_register))
        .route(
            "/api/localsend/v2/prepare-upload",
            post(handle_prepare_upload),
        )
        .route("/api/localsend/v2/upload", post(handle_upload))
        .route("/api/localsend/v2/cancel", post(handle_cancel))
        // The reserved credential is meaningful only on one exact protected
        // prepare route.  Never let another LocalSend handler silently ignore
        // it and fall back into standard behavior.
        .layer(middleware::from_fn(reject_crosscopy_header_on_other_routes))
        .with_state(state)
        .merge(web)
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
