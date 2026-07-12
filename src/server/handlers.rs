use super::state::{ServerState, write_body_to_file};
use crate::protocol::{DeviceInfo, FileId, PrepareUploadRequest, PrepareUploadResponse, SessionId};
use axum::{
    Json,
    body::Body,
    extract::{ConnectInfo, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Local;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) async fn handle_info(State(state): State<Arc<RwLock<ServerState>>>) -> Response {
    let state = state.read().await;
    Json(state.device.clone()).into_response()
}

pub(crate) async fn handle_register(
    State(state): State<Arc<RwLock<ServerState>>>,
    Json(remote_device): Json<DeviceInfo>,
) -> Response {
    tracing::debug!("Register request from {:?}", remote_device.alias);
    let state = state.read().await;
    Json(state.device.clone()).into_response()
}

#[derive(Deserialize)]
pub(crate) struct PrepareUploadParams {
    #[serde(rename = "pin")]
    pin: Option<String>,
}

pub(crate) async fn handle_prepare_upload(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Query(params): Query<PrepareUploadParams>,
    Json(request): Json<PrepareUploadRequest>,
) -> Response {
    // PIN gate runs first, before any session/event work -- a locked-out or
    // unauthenticated peer must never reach the accept flow (which would
    // otherwise answer with 403/409 instead of the correct 401/429).
    {
        let mut state = state_ref.write().await;
        match state.pin_gate.check(params.pin.as_deref(), peer.ip()) {
            crate::server::pin::PinVerdict::Ok => {}
            crate::server::pin::PinVerdict::Unauthorized => {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            crate::server::pin::PinVerdict::LockedOut => {
                return StatusCode::TOO_MANY_REQUESTS.into_response();
            }
        }
    }

    // R7: an empty files map means there is nothing to transfer -- answer
    // 204 immediately, before any session is reserved or accept-event is
    // emitted (a no-op request must not spuriously open a session).
    if request.files.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }

    // Check if it's a text message (all files have non-empty preview and small size)
    let is_message = !request.files.is_empty()
        && request
            .files
            .values()
            .all(|f| f.preview.is_some() && f.size < 1024 * 1024);

    // Short lock: reject a conflicting session, reserve this one with a
    // placeholder session over the *offered* files (replaced below with the
    // real session, built from the accepted files only, once the accept
    // decision is in), and pull out the config needed to make that decision.
    // Never hold this guard across the `timeout(...).await` below -- that
    // would deadlock every other concurrent request (including the upload
    // that follows acceptance).
    let (events_tx, auto_accept, accept_timeout) = {
        let mut state = state_ref.write().await;

        // Check for existing session timeout (e.g. 5 minutes or session finished)
        if let Some(session) = &state.current_session {
            if session.is_timed_out(300) {
                state.current_session = None;
            } else {
                tracing::warn!("Session already exists, rejecting new session");
                return StatusCode::CONFLICT.into_response();
            }
        }

        state.current_session = Some(crate::core::Session::new(
            request.info.alias.clone(),
            request.files.clone(),
        ));

        (
            state.events_tx.clone(),
            state.auto_accept,
            state.accept_timeout,
        )
    };

    // Decide: auto-accept, or ask the event consumer.
    let decision = if auto_accept {
        crate::server::events::TransferDecision::Accept
    } else {
        let (pending_request, decision_rx) =
            crate::server::events::PendingRequest::new(request.info.clone(), request.files.clone());
        if events_tx
            .send(crate::server::events::ServerEvent::TransferRequest(
                pending_request,
            ))
            .await
            .is_err()
        {
            // No consumer listening -> decline.
            crate::server::events::TransferDecision::Decline
        } else {
            match tokio::time::timeout(accept_timeout, decision_rx).await {
                Ok(Ok(d)) => d,
                _ => crate::server::events::TransferDecision::Decline, // dropped or timed out
            }
        }
    };

    let accepted_ids: Vec<FileId> = match decision {
        crate::server::events::TransferDecision::Accept => request.files.keys().cloned().collect(),
        crate::server::events::TransferDecision::AcceptFiles(ids) => ids
            .into_iter()
            .filter(|id| request.files.contains_key(id))
            .collect(),
        crate::server::events::TransferDecision::Decline => Vec::new(),
    };

    if accepted_ids.is_empty() {
        let mut state = state_ref.write().await;
        state.current_session = None;
        tracing::info!("Transfer declined (or timed out)");
        return StatusCode::FORBIDDEN.into_response();
    }

    // Build the real session from the accepted files only -- this replaces
    // the placeholder reservation above and generates fresh, random,
    // per-file tokens (R6: no longer derivable from session/file ids).
    let accepted_files: HashMap<FileId, crate::protocol::FileMetadata> = request
        .files
        .iter()
        .filter(|(id, _)| accepted_ids.contains(id))
        .map(|(id, meta)| (id.clone(), meta.clone()))
        .collect();
    let session = crate::core::Session::new(request.info.alias.clone(), accepted_files);
    let session_id = session.id.clone();
    let files_map = session.tokens.clone();

    {
        let mut state = state_ref.write().await;
        state.current_session = Some(session);
    }

    // If it's a message, return 204 No Content
    if is_message {
        let mut state = state_ref.write().await;

        // Save accepted messages to files. M1 fix: this branch used to save
        // the message with no ServerEvent at all, so a received text
        // message was written to disk but never reached the CLI event loop
        // or the TUI Inbox. Emit the same FileReceived/SessionDone pair
        // `handle_upload` emits for real files, against the session built
        // above for this request, so consumers see text messages the same
        // way they see file transfers.
        for (file_id, file) in &request.files {
            if !accepted_ids.contains(file_id) {
                continue;
            }
            if let Some(content) = &file.preview {
                let now = Local::now();
                let filename = format!(
                    "message_{}_{}.txt",
                    now.format("%Y%m%d_%H%M%S"),
                    file.file_name.replace("/", "_")
                );
                let path = match crate::core::unique_save_path(&state.save_dir, &filename) {
                    Ok(path) => path,
                    Err(e) => {
                        tracing::warn!("Rejected unsafe message file name: {}", e);
                        continue;
                    }
                };
                if let Err(e) = std::fs::write(&path, content) {
                    tracing::error!("Failed to save message to {:?}: {}", path, e);
                } else {
                    tracing::info!("Saved message to {:?}", path);

                    // Events must never block the request path: `try_send`,
                    // not `.send().await` (matches handle_upload). Report
                    // the *final* on-disk name -- unique_save_path may have
                    // renamed the file on collision, and a consumer needs
                    // to see where the bytes actually went.
                    let final_file_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| filename.clone());
                    let _ = state.events_tx.try_send(
                        crate::server::events::ServerEvent::FileReceived {
                            session_id: session_id.clone(),
                            file_id: file_id.clone(),
                            file_name: final_file_name,
                            path,
                            size: content.len() as u64,
                            sender_alias: request.info.alias.clone(),
                        },
                    );
                }
            }
        }

        let _ = state
            .events_tx
            .try_send(crate::server::events::ServerEvent::SessionDone {
                session_id: session_id.clone(),
            });

        state.current_session = None;
        return StatusCode::NO_CONTENT.into_response();
    }

    Json(PrepareUploadResponse {
        session_id,
        files: files_map,
    })
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct UploadParams {
    #[serde(rename = "sessionId")]
    session_id: SessionId,
    #[serde(rename = "fileId")]
    file_id: FileId,
    #[serde(rename = "token")]
    token: crate::protocol::Token,
}

#[axum::debug_handler]
pub(crate) async fn handle_upload(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(params): Query<UploadParams>,
    body: Body,
) -> Response {
    let state = state_ref.write().await;

    // Verify session
    let (file_name, session_id, sender_alias) = if let Some(session) = &state.current_session {
        if session.id != params.session_id {
            tracing::warn!(
                "Upload rejected: Session ID mismatch. Expected {}, got {}",
                session.id,
                params.session_id
            );
            return StatusCode::FORBIDDEN.into_response();
        }

        // Verify token against the session's random per-file token (R6) --
        // never re-derive it, only compare against what was issued.
        if !session.verify_token(&params.file_id, &params.token) {
            tracing::warn!("Upload rejected: Token mismatch");
            return StatusCode::FORBIDDEN.into_response();
        }

        // Find file metadata
        if let Some(meta) = session.files.get(&params.file_id) {
            (
                meta.file_name.clone(),
                session.id.clone(),
                session.sender_alias.clone(),
            )
        } else {
            tracing::warn!(
                "Upload rejected: File ID {} not found in session",
                params.file_id
            );
            return StatusCode::NOT_FOUND.into_response();
        }
    } else {
        tracing::warn!("Upload rejected: No active session");
        return StatusCode::FORBIDDEN.into_response();
    };

    let save_path = match crate::core::unique_save_path(&state.save_dir, &file_name) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!("Upload rejected: {}", e);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Release the lock before async I/O operations
    drop(state);

    // Ensure parent directory exists (async)
    if let Some(parent) = save_path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        tracing::error!("Failed to create directory {:?}: {}", parent, e);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let body_len = match write_body_to_file(body, &save_path).await {
        Ok(bytes_written) => bytes_written,
        Err(e) => {
            tracing::error!("Failed to save file to {:?}: {}", save_path, e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    tracing::info!("Received file: {:?} for session {}", save_path, session_id);

    // Reacquire lock for state updates
    let mut state = state_ref.write().await;

    // The write above was lock-free and may have taken a long time (large /
    // multi-GB files). The session validated before the write started may
    // have since been cancelled or timed out and replaced by a brand-new
    // session. Re-validate identity before touching session state: a stale
    // upload must never be recorded against a different session's
    // accounting, since a foreign file id could otherwise push an unrelated
    // session to "all done".
    let still_current = state
        .current_session
        .as_ref()
        .map(|session| session.id == session_id)
        .unwrap_or(false);

    // Record this file as received on the (still-current) session; a
    // multi-file transfer only closes once every accepted file has arrived,
    // not after the first one (R5).
    let all_done = if still_current {
        state
            .current_session
            .as_mut()
            .map(|session| session.mark_received(&params.file_id))
            .unwrap_or(false)
    } else {
        tracing::warn!(
            "Upload for session {} completed after the session was replaced; \
             file saved to disk but not recorded against the new session",
            session_id
        );
        false
    };

    // Events must never block the upload path: `try_send`, not `.send().await`
    // -- a slow or absent event consumer must not stall the transfer.
    // The bytes genuinely landed on disk, so FileReceived is still accurate
    // to emit even if the owning session has since changed. Report the
    // *final* on-disk name -- unique_save_path may have renamed the file on
    // collision, and a consumer needs to see where the bytes actually went,
    // not the name originally requested by the sender.
    let final_file_name = save_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(file_name);
    let _ = state
        .events_tx
        .try_send(crate::server::events::ServerEvent::FileReceived {
            session_id: session_id.clone(),
            file_id: params.file_id.clone(),
            file_name: final_file_name,
            path: save_path,
            size: body_len,
            sender_alias,
        });

    if still_current && all_done {
        let _ = state
            .events_tx
            .try_send(crate::server::events::ServerEvent::SessionDone { session_id });
        state.current_session = None;
    }

    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
pub(crate) struct CancelParams {
    #[serde(rename = "sessionId")]
    session_id: SessionId,
}

pub(crate) async fn handle_cancel(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(params): Query<CancelParams>,
) -> Response {
    let mut state = state_ref.write().await;

    if let Some(session) = &state.current_session
        && session.id.as_str() == params.session_id.as_str()
    {
        let _ = state
            .events_tx
            .try_send(crate::server::events::ServerEvent::SessionDone {
                session_id: params.session_id.clone(),
            });
        state.current_session = None;
        tracing::info!("Session {} cancelled", params.session_id);
    }

    StatusCode::OK.into_response()
}
