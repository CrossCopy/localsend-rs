use super::crosscopy_authorized::{
    CROSSCOPY_FILE_V3_HANDOFF_HEADER, CrossCopyAuthorizedHandoff, CrossCopyAuthorizedPrepare,
    CrossCopyAuthorizedPrepareMetadata, CrossCopyAuthorizedUpload, CrossCopyAuthorizedUploadBody,
};
use super::events::ServerEvent;
use super::state::{CrossCopyAuthorizedSession, ServerState, write_body_to_file_with_progress};
use crate::protocol::{DeviceInfo, FileId, PrepareUploadRequest, PrepareUploadResponse, SessionId};
use axum::{
    Json,
    body::Body,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

pub(crate) async fn handle_info(State(state): State<Arc<RwLock<ServerState>>>) -> Response {
    let state = state.read().await;
    Json(state.device.clone()).into_response()
}

pub(crate) async fn handle_register(
    State(state): State<Arc<RwLock<ServerState>>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Json(mut remote_device): Json<DeviceInfo>,
) -> Response {
    tracing::debug!("Register request from {:?}", remote_device.alias);
    let state = state.read().await;
    // Prefer the address we actually received the request from over the one the
    // body claims. The socket cannot be wished into being wrong, and the
    // official client omits `ip` from what it posts anyway.
    remote_device.ip = Some(peer.ip().to_string());
    // A registration is a sighting: this peer was reachable a moment ago, and
    // said so itself. Announcing it costs nothing when nobody is listening.
    let _ = state
        .events_tx
        .try_send(ServerEvent::PeerRegistered(remote_device));
    Json(state.device.clone()).into_response()
}

#[derive(Deserialize)]
pub(crate) struct PrepareUploadParams {
    #[serde(rename = "pin")]
    pin: Option<String>,
}

/// Releases a consent reservation if the request that made it goes away.
///
/// Dropped on every exit from `prepare-upload`, including the one that is not a
/// return: an axum handler whose client disconnected is simply dropped, and
/// before this the placeholder it had installed stayed until the sweep.
struct ReservationGuard {
    state: Arc<RwLock<ServerState>>,
    reserved: SessionId,
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        let state = self.state.clone();
        let reserved = self.reserved.clone();
        // `Drop` cannot await, and taking the lock is the whole job. A task is
        // the only way, and it is cheap: it runs once and only if the
        // reservation is still this one.
        tokio::spawn(async move {
            let mut state = state.write().await;
            let ours = state
                .awaiting_consent
                .as_ref()
                .is_some_and(|reservation| reservation.session_id == reserved);
            if !ours {
                return;
            }
            state.awaiting_consent = None;
            if let Some(session) = &state.current_session
                && session.id == reserved
            {
                let _ = session.try_cancel_receives();
                state.current_session = None;
                state.current_session_from = None;
                tracing::info!(%reserved, "Released a reservation nobody is waiting on any more");
            }
        });
    }
}

pub(crate) async fn handle_prepare_upload(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Query(params): Query<PrepareUploadParams>,
    headers: HeaderMap,
    Json(request): Json<PrepareUploadRequest>,
) -> Response {
    if headers.contains_key(CROSSCOPY_FILE_V3_HANDOFF_HEADER) {
        return handle_crosscopy_authorized_prepare(state_ref, headers, request).await;
    }
    handle_standard_prepare_upload(state_ref, peer, params, request).await
}

async fn handle_crosscopy_authorized_prepare(
    state_ref: Arc<RwLock<ServerState>>,
    headers: HeaderMap,
    request: PrepareUploadRequest,
) -> Response {
    // Header names are case-insensitive through `HeaderMap`, but the value is
    // intentionally stricter: exactly one canonical lowercase hex token with
    // no whitespace, commas, or alternate encoding.
    let values = match headers
        .get_all(CROSSCOPY_FILE_V3_HANDOFF_HEADER)
        .iter()
        .map(|value| value.to_str())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(values) => values,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let handoff = match CrossCopyAuthorizedHandoff::parse(&values) {
        Ok(handoff) => handoff,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // File-v3 is one exact regular file.  Text/mixed/multi-file LocalSend
    // shapes never enter the protected mode and cannot cause standard fallback.
    if request.files.len() != 1 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some((file_id, file)) = request.files.into_iter().next() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if file.file_name.trim().is_empty() || file.preview.is_some() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let metadata = CrossCopyAuthorizedPrepareMetadata::new(request.info, file_id, file);

    // Avoid consuming an external, linear File-v3 receiver slot when this
    // listener already has a protected upload in flight.  This is only a
    // preflight: a concurrent prepare can still win the race after this read,
    // so the post-take insertion below remains responsible for terminalizing
    // the owner it cannot install.
    let (stopping, occupied) = {
        let state = state_ref.read().await;
        (
            state.crosscopy_authorized_stopping,
            state.crosscopy_authorized_session.is_some(),
        )
    };
    if stopping {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if occupied {
        return StatusCode::CONFLICT.into_response();
    }

    let gate = state_ref
        .read()
        .await
        .crosscopy_authorized_upload_gate
        .clone();
    let Some(gate) = gate else {
        // The default listener intentionally has no File-v3 authority.
        return StatusCode::FORBIDDEN.into_response();
    };
    let owner = match gate
        .take_authorized_upload(CrossCopyAuthorizedPrepare::new(handoff, metadata.clone()))
        .await
    {
        Ok(owner) => owner,
        Err(_) => return StatusCode::FORBIDDEN.into_response(),
    };
    let session = CrossCopyAuthorizedSession::new(metadata, owner);
    let response = PrepareUploadResponse {
        session_id: session.session_id.clone(),
        files: std::iter::once((session.file_id.clone(), session.upload_token.clone())).collect(),
    };
    let rejected = {
        let mut state = state_ref.write().await;
        if state.crosscopy_authorized_stopping {
            Some((session.owner, StatusCode::SERVICE_UNAVAILABLE))
        } else if state.crosscopy_authorized_session.is_some() {
            Some((session.owner, StatusCode::CONFLICT))
        } else {
            state.crosscopy_authorized_session = Some(session);
            None
        }
    };
    if let Some((owner, status)) = rejected {
        // The external gate already consumed a linear slot.  A local session
        // conflict or concurrent orderly shutdown must terminalize that owner
        // instead of leaking it.
        owner.cancel().await;
        return status.into_response();
    }
    Json(response).into_response()
}

async fn handle_standard_prepare_upload(
    state_ref: Arc<RwLock<ServerState>>,
    peer: std::net::SocketAddr,
    params: PrepareUploadParams,
    request: PrepareUploadRequest,
) -> Response {
    // PIN gate runs first, before any session/event work -- a locked-out or
    // unauthenticated peer must never reach the accept flow (which would
    // otherwise answer with 403/409 instead of the correct 401/429).
    {
        let mut state = state_ref.write().await;
        match state.pin_gate.check(params.pin.as_deref(), peer.ip()) {
            crate::server::pin::PinVerdict::Ok { .. } => {}
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

    // LocalSend represents a text message as exactly one small offered item
    // whose non-empty `preview` is the complete body. Mixed/multi-file offers
    // remain ordinary file transfers even if one item happens to have preview
    // metadata.
    let message_text = if request.files.len() == 1 {
        request.files.values().next().and_then(|file| {
            file.preview
                .as_ref()
                .filter(|text| !text.is_empty() && file.size < 1024 * 1024)
                .cloned()
        })
    } else {
        None
    };

    // Short lock: reject a conflicting session, reserve this one with a
    // placeholder session over the *offered* files (replaced below with the
    // real session, built from the accepted files only, once the accept
    // decision is in), and pull out the config needed to make that decision.
    // Never hold this guard across the `timeout(...).await` below -- that
    // would deadlock every other concurrent request (including the upload
    // that follows acceptance).
    // Declared outside the lock so the guard below can name them.
    let reserved: SessionId;
    let withdraw: tokio_util::sync::CancellationToken;

    let (events_tx, auto_accept, accept_timeout) = {
        let mut state = state_ref.write().await;

        // Check for existing session timeout (e.g. 5 minutes or session finished)
        if let Some(session) = &state.current_session {
            if session.is_timed_out(state.session_timeout) {
                if session.try_cancel_receives() {
                    state.current_session = None;
                } else {
                    tracing::warn!("Timed-out session is publishing a file; rejecting replacement");
                    return StatusCode::CONFLICT.into_response();
                }
            } else {
                tracing::warn!("Session already exists, rejecting new session");
                return StatusCode::CONFLICT.into_response();
            }
        }

        let placeholder =
            crate::core::Session::new(request.info.alias.clone(), request.files.clone());
        reserved = placeholder.id.clone();
        withdraw = tokio_util::sync::CancellationToken::new();
        state.awaiting_consent = Some(crate::server::state::ConsentReservation {
            session_id: reserved.clone(),
            from: peer.ip(),
            withdraw: withdraw.clone(),
        });
        state.current_session_from = Some(peer.ip());
        state.current_session = Some(placeholder);

        (
            state.events_tx.clone(),
            state.auto_accept.load(std::sync::atomic::Ordering::Relaxed),
            state.accept_timeout,
        )
    };

    // **The reservation is released if this future is dropped.** A sender that
    // closes the connection while somebody is deciding used to leave the
    // placeholder in place until the sweep reclaimed it — up to
    // `session_timeout`, default five minutes — and every other LocalSend peer
    // on the network got 409 in the meantime. Nothing woke that up, because a
    // dropped axum handler runs no more code of its own.
    //
    // The guard is keyed on the reservation id and checks it before clearing,
    // so a guard that outlives its own decision cannot cancel somebody else's
    // session.
    let _release = ReservationGuard {
        state: state_ref.clone(),
        reserved: reserved.clone(),
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
            tokio::select! {
                biased;
                // The sender said `/cancel` before anybody answered. Treat it
                // as a decline so the one unwind path below still runs.
                _ = withdraw.cancelled() => {
                    tracing::info!("Sender withdrew its offer before it was answered");
                    crate::server::events::TransferDecision::Decline
                }
                answer = tokio::time::timeout(accept_timeout, decision_rx) => match answer {
                    Ok(Ok(d)) => d,
                    _ => crate::server::events::TransferDecision::Decline, // dropped or timed out
                },
            }
        }
    };

    {
        // The answer arrived, so this is no longer a reservation waiting on one.
        // Clearing it here rather than in the guard is what makes the guard's
        // id check meaningful: past this line the guard finds nothing of its
        // own and does nothing.
        let mut state = state_ref.write().await;
        if state
            .awaiting_consent
            .as_ref()
            .is_some_and(|reservation| reservation.session_id == reserved)
        {
            state.awaiting_consent = None;
        }
    }

    // A refusal is not a decline and unwinds on its own path, because the two
    // answer the sender differently and only one of them is worth retrying.
    if let crate::server::events::TransferDecision::Refuse { reason } = &decision {
        let mut state = state_ref.write().await;
        if let Some(session) = &state.current_session {
            let _ = session.try_cancel_receives();
        }
        state.current_session = None;
        tracing::warn!(%reason, "Offer refused as unusable");
        return StatusCode::BAD_REQUEST.into_response();
    }

    let accepted_ids: Vec<FileId> = match decision {
        crate::server::events::TransferDecision::Accept => request.files.keys().cloned().collect(),
        crate::server::events::TransferDecision::AcceptFiles(ids) => ids
            .into_iter()
            .filter(|id| request.files.contains_key(id))
            .collect(),
        crate::server::events::TransferDecision::Decline => Vec::new(),
        crate::server::events::TransferDecision::Refuse { .. } => unreachable!("handled above"),
    };

    if accepted_ids.is_empty() {
        let mut state = state_ref.write().await;
        if let Some(session) = &state.current_session {
            let _ = session.try_cancel_receives();
        }
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
        if let Some(placeholder) = &state.current_session {
            let _ = placeholder.try_cancel_receives();
        }
        state.current_session = Some(session);
    }

    // If it's a message, return 204 No Content
    if let Some(text) = message_text {
        let mut state = state_ref.write().await;
        let _ = state
            .events_tx
            .try_send(crate::server::events::ServerEvent::TextReceived {
                session_id: session_id.clone(),
                text,
                sender_alias: request.info.alias.clone(),
            });

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

#[derive(Clone)]
struct ReceiveProgressContext {
    session_id: SessionId,
    file_id: FileId,
    file_name: String,
    sender_alias: String,
    total_bytes: u64,
    file_count: usize,
    events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
    received_bytes: Arc<AtomicU64>,
}

impl ReceiveProgressContext {
    fn add(&self, amount: u64) {
        let previous = self
            .received_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(amount).min(self.total_bytes))
            })
            .unwrap_or_else(|current| current);
        self.emit(previous.saturating_add(amount).min(self.total_bytes));
    }

    fn rollback(&self, amount: u64) {
        let previous = self
            .received_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(amount))
            })
            .unwrap_or_else(|current| current);
        self.emit(previous.saturating_sub(amount));
    }

    fn emit(&self, bytes_received: u64) {
        let _ = self
            .events_tx
            .try_send(crate::server::events::ServerEvent::FileReceiveProgress {
                session_id: self.session_id.clone(),
                file_id: self.file_id.clone(),
                file_name: self.file_name.clone(),
                sender_alias: self.sender_alias.clone(),
                bytes_received,
                total_bytes: self.total_bytes,
                file_count: self.file_count,
            });
    }
}

#[axum::debug_handler]
pub(crate) async fn handle_upload(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(params): Query<UploadParams>,
    body: Body,
) -> Response {
    let mut state = state_ref.write().await;

    // The protected session owns a distinct one-shot token and opaque sink.
    // Check it before the standard state, remove it atomically, and never pass
    // its body through the standard save/event pipeline.
    if state
        .crosscopy_authorized_session
        .as_ref()
        .is_some_and(|session| session.session_id == params.session_id)
    {
        let session = state
            .crosscopy_authorized_session
            .take()
            .expect("protected session remained present under write lock");
        let events_tx = state.events_tx.clone();
        if session.file_id != params.file_id || session.upload_token != params.token {
            drop(state);
            session.owner.cancel().await;
            return StatusCode::FORBIDDEN.into_response();
        }
        let active_session_id = session.session_id.clone();
        state.crosscopy_authorized_active_upload =
            Some(super::state::CrossCopyAuthorizedActiveUpload {
                session_id: active_session_id.clone(),
                cancellation: session.owner.cancellation(),
            });
        drop(state);
        let stream = body
            .into_data_stream()
            .map(|item| item.map_err(|error| std::io::Error::other(error.to_string())));
        let sender_alias = session.metadata.sender().alias.clone();
        let file_id = session.metadata.file_id().clone();
        let file_name = session.metadata.file().file_name.clone();
        let session_id = session.session_id.clone();
        let upload = CrossCopyAuthorizedUpload::new(
            session.session_id,
            session.metadata,
            CrossCopyAuthorizedUploadBody::new(Box::pin(stream)),
        );
        let result = session.owner.receive(upload).await;
        {
            let mut state = state_ref.write().await;
            if state
                .crosscopy_authorized_active_upload
                .as_ref()
                .is_some_and(|active| active.session_id == active_session_id)
            {
                state.crosscopy_authorized_active_upload = None;
            }
        }
        return match result {
            Ok(receipt) => {
                let _ = events_tx
                    .send(ServerEvent::FileReceived {
                        session_id: session_id.clone(),
                        file_id,
                        file_name,
                        path: receipt.path().clone(),
                        size: receipt.size(),
                        sender_alias,
                        message_text: None,
                    })
                    .await;
                let _ = events_tx
                    .send(ServerEvent::SessionDone { session_id })
                    .await;
                StatusCode::OK.into_response()
            }
            // The host's reason is the only account of WHY a protected upload
            // failed, and this is the last place it exists: the 500 that goes
            // back on the wire carries no detail, and the sender can only report
            // "File upload failed". Discarding it here once cost six instrumented
            // runs to recover a single `DestinationExists`.
            Err(error) => {
                tracing::warn!(
                    %error,
                    %session_id,
                    "protected File-v3 upload was refused by the host gate"
                );
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }

    // Verify session
    let (
        file_name,
        session_id,
        sender_alias,
        declared_size,
        declared_sha,
        events_tx,
        received_bytes,
        total_bytes,
        file_count,
        receive_rate_limit_bytes_per_second,
        save_dir,
        sink,
        receive_lease,
    ) = if let Some(session) = &state.current_session {
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
            let Some(receive_lease) = session.begin_receive(&params.file_id) else {
                tracing::warn!(
                    "Upload rejected: file {} is already receiving, published, or cancelled",
                    params.file_id
                );
                return StatusCode::CONFLICT.into_response();
            };
            (
                meta.file_name.clone(),
                session.id.clone(),
                session.sender_alias.clone(),
                meta.size,
                meta.sha256.clone(),
                state.events_tx.clone(),
                session.received_bytes.clone(),
                session
                    .files
                    .values()
                    .map(|file| file.size)
                    .fold(0_u64, u64::saturating_add),
                session.files.len(),
                state.receive_rate_limit_bytes_per_second,
                state.save_dir.clone(),
                state.sink.clone(),
                receive_lease,
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

    // Release the session lock before filesystem work or body I/O. Name
    // selection and exclusive creation happen together below, relative to a
    // pinned receiver-selected root.
    drop(state);

    // The sink's error is classified rather than opaque, because the two halves
    // land on different status codes and the LocalSend spec's upload table
    // treats them as different things: 400 is "you sent something wrong", 500
    // is "this receiver broke". Collapsing them would tell a sender that named
    // `../../etc/passwd` to retry.
    let mut pending = match sink.create(&save_dir, &file_name).await {
        Ok(pending) => pending,
        Err(crate::core::SinkError::Rejected(reason)) => {
            tracing::warn!(%reason, "Upload rejected: unusable remote file name {file_name:?}");
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(error) => {
            tracing::error!(%error, "Failed to create a receive destination");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let save_path = pending.display_path().to_owned();

    let progress = ReceiveProgressContext {
        session_id: session_id.clone(),
        file_id: params.file_id.clone(),
        file_name: file_name.clone(),
        sender_alias: sender_alias.clone(),
        total_bytes,
        file_count,
        events_tx,
        received_bytes,
    };
    let callback_progress = progress.clone();
    let file_reported = Arc::new(AtomicU64::new(0));
    let callback_file_reported = file_reported.clone();
    let mut previous_file_bytes = 0_u64;
    let cancellation = receive_lease.cancellation();
    let body_result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = write_body_to_file_with_progress(
            body,
            pending.writer(),
            receive_rate_limit_bytes_per_second,
            move |file_bytes| {
                let delta = file_bytes.saturating_sub(previous_file_bytes);
                previous_file_bytes = file_bytes;
                callback_file_reported.store(file_bytes, Ordering::Relaxed);
                callback_progress.add(delta);
            },
        ) => Some(result),
    };
    let body = match body_result {
        None => {
            progress.rollback(file_reported.load(Ordering::Relaxed));
            tracing::info!(%session_id, "Upload cancelled before publication");
            if let Err(cleanup_error) = pending.abort().await {
                tracing::error!(%cleanup_error, "Failed to clean up cancelled upload");
            }
            return StatusCode::CONFLICT.into_response();
        }
        Some(Ok(outcome)) => outcome,
        Some(Err(e)) => {
            progress.rollback(file_reported.load(Ordering::Relaxed));
            tracing::error!("Failed to save file to {:?}: {}", save_path, e);
            if let Err(cleanup_error) = pending.abort().await {
                tracing::error!(%cleanup_error, "Failed to clean up rejected upload");
            }
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let body_len = body.bytes_written;

    // Validate the received bytes against the metadata declared in
    // prepare-upload. A truncated body (network cut, or a misbehaving client
    // that illegally splits the upload into multiple POSTs) would otherwise
    // be saved as a partial file and the session wrongly marked complete.
    // On any mismatch: discard the partial, return 500 ("Unknown error by
    // receiver", per the LocalSend v2.1 spec's upload error table), and leave
    // the session untouched so it is neither recorded nor completed -- the
    // sender can retry the same file id against the still-open session.
    if body_len != declared_size {
        progress.rollback(body_len);
        tracing::warn!(
            "Upload size mismatch for {:?}: declared {} bytes, received {} bytes; discarding partial",
            save_path,
            declared_size,
            body_len
        );
        if let Err(cleanup_error) = pending.abort().await {
            tracing::error!(%cleanup_error, "Failed to clean up size-mismatched upload");
        }
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // When the sender advertised a sha256, verify the bytes on disk match it
    // (case-insensitive hex). Size can be right while the contents are
    // corrupt; reject those the same way.
    if let Some(expected_sha) = declared_sha
        && !body.sha256.eq_ignore_ascii_case(&expected_sha)
    {
        let actual = &body.sha256;
        progress.rollback(body_len);
        tracing::warn!(
            "Upload sha256 mismatch for {:?}: declared {}, computed {}; discarding",
            save_path,
            expected_sha,
            actual
        );
        if let Err(cleanup_error) = pending.abort().await {
            tracing::error!(%cleanup_error, "Failed to clean up hash-mismatched upload");
        }
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let Some(publication) = receive_lease.begin_publication() else {
        progress.rollback(body_len);
        tracing::info!(%session_id, "Upload cancelled before publication");
        if let Err(cleanup_error) = pending.abort().await {
            tracing::error!(%cleanup_error, "Failed to clean up cancelled upload");
        }
        return StatusCode::CONFLICT.into_response();
    };

    let save_path = match pending.commit().await {
        Ok(path) => path,
        Err(error) => {
            progress.rollback(body_len);
            tracing::error!(%error, "Failed to commit safe receive destination");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    tracing::info!("Received file: {:?} for session {}", save_path, session_id);

    // Reacquire lock for state updates
    let mut state = state_ref.write().await;

    // The publication permit prevents cancellation/replacement from clearing
    // this exact session between the atomic filesystem commit and its state /
    // event transition.
    let still_current = state
        .current_session
        .as_ref()
        .map(|session| session.id == session_id)
        .unwrap_or(false);

    // Record this file as received on the (still-current) session; a
    // multi-file transfer only closes once every accepted file has arrived,
    // not after the first one (R5).
    if !still_current {
        tracing::error!(
            %session_id,
            "Receive lifecycle invariant violated after publication"
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let all_done = state
        .current_session
        .as_mut()
        .expect("publication permit keeps its session installed")
        .mark_received(&params.file_id);

    // Events must never block the upload path: `try_send`, not `.send().await`
    // -- a slow or absent event consumer must not stall the transfer.
    // Report the *final* on-disk name -- the atomic materializer may have renamed the
    // file on collision, and a consumer needs to see where the bytes actually
    // went, not the name originally requested by the sender.
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
            // A real binary upload has no inline text body.
            message_text: None,
        });

    publication.finish();

    if all_done {
        let _ = state
            .events_tx
            .try_send(crate::server::events::ServerEvent::SessionDone { session_id });
        state.current_session = None;
    }

    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
pub(crate) struct CancelParams {
    /// **Optional, and that is the spec's shape rather than laxity.** The
    /// official receiver requires a session id only when it is *not* in the
    /// waiting state: a sender that gives up while somebody is still deciding
    /// has never been told a session id, because the reservation's id is
    /// internal until the offer is accepted.
    #[serde(rename = "sessionId")]
    session_id: Option<SessionId>,
}

pub(crate) async fn handle_cancel(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Query(params): Query<CancelParams>,
) -> Response {
    let mut state = state_ref.write().await;

    // **Authorized by source IP, not by knowing the session id.** That is what
    // the official receiver does, and the difference matters on a shared
    // network: a session id travels in a `prepare-upload` response over plain
    // HTTP by default, so treating possession of one as authority to cancel
    // hands the transfer to anybody who watched it go past.
    let claimed_by_sender = state
        .current_session_from
        .is_some_and(|reserved_by| reserved_by == peer.ip());

    // The sender gave up while somebody was still deciding. It has no session
    // id to name, so not naming one is the request rather than a malformed one.
    if params.session_id.is_none() {
        let Some(reservation) = state.awaiting_consent.take() else {
            tracing::warn!("Cancel with no session id, and nothing is awaiting consent");
            return StatusCode::BAD_REQUEST.into_response();
        };
        if reservation.from != peer.ip() {
            state.awaiting_consent = Some(reservation);
            tracing::warn!("Cancel from {} is not the peer that reserved", peer.ip());
            return StatusCode::FORBIDDEN.into_response();
        }
        if let Some(session) = &state.current_session
            && session.id == reservation.session_id
        {
            let _ = session.try_cancel_receives();
            state.current_session = None;
            state.current_session_from = None;
        }
        // Wakes the parked handler, which unwinds through its own decline path.
        reservation.withdraw.cancel();
        tracing::info!("Sender withdrew its offer before it was answered");
        return StatusCode::OK.into_response();
    }
    let session_id = params
        .session_id
        .clone()
        .expect("the None arm returned above");

    if !claimed_by_sender && state.current_session_from.is_some() {
        tracing::warn!("Cancel from {} is not the peer that reserved", peer.ip());
        return StatusCode::FORBIDDEN.into_response();
    }

    if state
        .crosscopy_authorized_session
        .as_ref()
        .is_some_and(|session| session.session_id == session_id)
    {
        let session = state
            .crosscopy_authorized_session
            .take()
            .expect("protected session remained present under write lock");
        drop(state);
        session.owner.cancel().await;
        return StatusCode::OK.into_response();
    }

    if state
        .crosscopy_authorized_active_upload
        .as_ref()
        .is_some_and(|active| active.session_id == session_id)
    {
        state
            .crosscopy_authorized_active_upload
            .as_ref()
            .expect("active protected upload remained present")
            .cancellation
            .cancel();
        return StatusCode::OK.into_response();
    }

    if let Some(session) = &state.current_session
        && session.id == session_id
    {
        if !session.try_cancel_receives() {
            tracing::info!(
                "Session {} is already publishing and cannot be cancelled",
                session_id
            );
            return StatusCode::CONFLICT.into_response();
        }
        let _ = state
            .events_tx
            .try_send(crate::server::events::ServerEvent::SessionDone {
                session_id: session_id.clone(),
            });
        state.current_session = None;
        state.current_session_from = None;
        tracing::info!("Session {} cancelled", session_id);
    }

    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Protocol;

    /// A cancel is authorized by **source IP**, and this is the row that says
    /// knowing the session id is not enough.
    ///
    /// It is a unit test rather than one over the wire because two source
    /// addresses on loopback is a platform question — Linux gives you the whole
    /// `127.0.0.0/8`, macOS aliases only `127.0.0.1` — and a security property
    /// should not be asserted only where the aliasing happens to work.
    #[tokio::test]
    async fn a_cancel_from_a_different_peer_is_refused_even_with_the_right_session_id() {
        let (events_tx, _events_rx) = tokio::sync::mpsc::channel(8);
        let session = crate::core::Session::new("sender".to_string(), HashMap::new());
        let session_id = session.id.clone();
        let device = DeviceInfo::new("receiver".to_string(), 53317, Protocol::Http);
        let state = Arc::new(RwLock::new(ServerState {
            device: device.clone(),
            current_session: Some(session),
            current_session_from: Some("10.0.0.5".parse().unwrap()),
            awaiting_consent: None,
            save_dir: std::path::PathBuf::from("."),
            events_tx: events_tx.clone(),
            auto_accept: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            accept_timeout: std::time::Duration::from_secs(60),
            session_timeout: std::time::Duration::from_secs(300),
            receive_rate_limit_bytes_per_second: None,
            pin_gate: crate::server::pin::PinGate::new(None),
            web: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::web_share::WebShareHost::new(
                    device,
                    events_tx,
                    std::time::Duration::from_secs(30),
                ),
            )),
            sink: Arc::new(crate::core::AtomicFileSink),
            crosscopy_authorized_upload_gate: None,
            crosscopy_authorized_session: None,
            crosscopy_authorized_active_upload: None,
            crosscopy_authorized_stopping: false,
        }));

        let stranger = handle_cancel(
            State(state.clone()),
            ConnectInfo("10.0.0.9:40000".parse().unwrap()),
            Query(CancelParams {
                session_id: Some(session_id.clone()),
            }),
        )
        .await;
        assert_eq!(stranger.status(), StatusCode::FORBIDDEN);
        assert!(
            state.read().await.current_session.is_some(),
            "a stranger cancelled somebody else's session"
        );

        let sender = handle_cancel(
            State(state.clone()),
            ConnectInfo("10.0.0.5:40000".parse().unwrap()),
            Query(CancelParams {
                session_id: Some(session_id),
            }),
        )
        .await;
        assert_eq!(sender.status(), StatusCode::OK);
        assert!(state.read().await.current_session.is_none());
    }
}
