use super::state::{ActiveSession, ServerState, write_body_to_file};
use crate::protocol::{
    DeviceInfo, FileId, PrepareUploadRequest, PrepareUploadResponse, ReceivedFile, SessionId,
};
use axum::{
    Json,
    body::Body,
    extract::{Query, State},
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
    _pin: Option<String>,
}

pub(crate) async fn handle_prepare_upload(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(_params): Query<PrepareUploadParams>,
    Json(request): Json<PrepareUploadRequest>,
) -> Response {
    use crate::protocol::{SessionId, Token};

    let session_id = SessionId::new();

    // Check if it's a text message (all files have non-empty preview and small size)
    let is_message = !request.files.is_empty()
        && request
            .files
            .values()
            .all(|f| f.preview.is_some() && f.size < 1024 * 1024);

    // Short lock: reject a conflicting session, reserve this one, and pull out
    // the config needed to make the accept decision. Never hold this guard
    // across the `timeout(...).await` below -- that would deadlock every other
    // concurrent request (including the upload that follows acceptance).
    let (events_tx, auto_accept, accept_timeout) = {
        let mut state = state_ref.write().await;

        // Check for existing session timeout (e.g. 5 minutes or session finished)
        if let Some(session) = &state.current_session {
            if session.last_activity.elapsed().as_secs() > 300 {
                state.current_session = None;
            } else {
                tracing::warn!("Session already exists, rejecting new session");
                return StatusCode::CONFLICT.into_response();
            }
        }

        let session = ActiveSession {
            session_id: session_id.clone(),
            files: request.files.clone(),
            sender_alias: request.info.alias.clone(),
            last_activity: std::time::Instant::now(),
        };

        state.current_session = Some(session);

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

    // Build the response token map from the accepted files only, and narrow
    // the stored session down to the same set.
    let mut files_map = HashMap::new();
    for file_id in &accepted_ids {
        let token = Token::new(&session_id, file_id);
        files_map.insert(file_id.clone(), token);
    }

    {
        let mut state = state_ref.write().await;
        if let Some(s) = &mut state.current_session {
            s.files.retain(|id, _| accepted_ids.contains(id));
            s.last_activity = std::time::Instant::now();
        }
    }

    // If it's a message, return 204 No Content
    if is_message {
        let mut state = state_ref.write().await;

        // Save accepted messages to files and update TUI list
        for (file_id, file) in &request.files {
            if !accepted_ids.contains(file_id) {
                continue;
            }
            if let Some(content) = &file.preview {
                let now = Local::now();
                let time_str = now.format("%Y-%m-%d %H:%M:%S").to_string();
                let filename = format!(
                    "message_{}_{}.txt",
                    now.format("%Y%m%d_%H%M%S"),
                    file.file_name.replace("/", "_")
                );
                let path = match crate::path_safety::safe_join(&state.save_dir, &filename) {
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

                    // Update TUI list
                    let mut files_list = state.received_files.write().await;
                    files_list.push(ReceivedFile {
                        file_name: filename,
                        size: content.len() as u64,
                        sender: request.info.alias.clone(),
                        time: time_str,
                    });
                }
            }
        }

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
    let (file_name, session_id) = if let Some(session) = &state.current_session {
        if session.session_id != params.session_id {
            tracing::warn!(
                "Upload rejected: Session ID mismatch. Expected {}, got {}",
                session.session_id,
                params.session_id
            );
            return StatusCode::FORBIDDEN.into_response();
        }

        // Verify token
        let expected_token = crate::protocol::Token::new(&session.session_id, &params.file_id);
        if params.token.as_str() != expected_token.as_str() {
            tracing::warn!("Upload rejected: Token mismatch");
            return StatusCode::FORBIDDEN.into_response();
        }

        // Find file metadata
        if let Some(meta) = session.files.get(&params.file_id) {
            (meta.file_name.clone(), session.session_id.clone())
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

    let save_path = match crate::path_safety::safe_join(&state.save_dir, &file_name) {
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

    // Update TUI list
    {
        let sender = state
            .current_session
            .as_ref()
            .map(|s| s.sender_alias.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        let time_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let mut files_list = state.received_files.write().await;
        files_list.push(ReceivedFile {
            file_name,
            size: body_len,
            sender,
            time: time_str,
        });
    }

    // Update last activity and check if session is complete (simple heuristic: 1 file for now)
    // In a real LocalSend implementation, we'd wait for all files.
    if let Some(s) = &mut state.current_session {
        s.last_activity = std::time::Instant::now();
        // For simplicity, we clear session after one file if it was the only one
        if s.files.len() <= 1 {
            state.current_session = None;
        }
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
        && session.session_id.as_str() == params.session_id.as_str()
    {
        state.current_session = None;
        tracing::info!("Session {} cancelled", params.session_id);
    }

    StatusCode::OK.into_response()
}
