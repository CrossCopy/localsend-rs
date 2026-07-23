//! The optional, fail-closed CrossCopy-authorized LocalSend receive mode.
//!
//! This module intentionally knows neither Fabric nor the File-service
//! database.  A host injects a narrow gate which may consume one authorized
//! rendezvous and return one opaque upload owner.  Without that gate the HTTP
//! server remains a normal LocalSend receiver and rejects the reserved header.

use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::protocol::{DeviceInfo, FileId, FileMetadata, SessionId};

/// The only HTTP credential accepted by the protected LocalSend mode.
pub const CROSSCOPY_FILE_V3_HANDOFF_HEADER: &str = "x-crosscopy-file-v3-handoff";

/// Redacting, canonical receiver-issued handoff credential.
///
/// The value remains a redacting domain wrapper rather than a generic string.
/// A trusted gate can borrow it for its one slot lookup, but cannot obtain it
/// from a LocalSend response, URL, diagnostic, or session record.
pub struct CrossCopyAuthorizedHandoff(String);

impl CrossCopyAuthorizedHandoff {
    pub(crate) fn parse(values: &[&str]) -> Result<Self, CrossCopyAuthorizedUploadError> {
        if values.len() != 1 {
            return Err(CrossCopyAuthorizedUploadError::InvalidHandoff);
        }
        let value = values[0];
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(CrossCopyAuthorizedUploadError::InvalidHandoff);
        }
        Ok(Self(value.to_owned()))
    }

    /// Run one gate operation with a borrowed canonical value.  The operation
    /// may return a future, so an adapter can keep this wrapper alive across an
    /// async slot lookup without receiving a string getter or ownership.
    pub fn with_value<T>(&self, operation: impl FnOnce(&str) -> T) -> T {
        operation(&self.0)
    }
}

impl std::fmt::Debug for CrossCopyAuthorizedHandoff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CrossCopyAuthorizedHandoff([REDACTED])")
    }
}

/// Exact one-file LocalSend metadata that the protected gate may compare to
/// receiver-owned authorization facts.  It is constructed only by the HTTP
/// handler after it rejects multi-file and text-shaped requests.
#[derive(Clone, Debug)]
pub struct CrossCopyAuthorizedPrepareMetadata {
    sender: DeviceInfo,
    file_id: FileId,
    file: FileMetadata,
}

impl CrossCopyAuthorizedPrepareMetadata {
    pub(crate) fn new(sender: DeviceInfo, file_id: FileId, file: FileMetadata) -> Self {
        Self {
            sender,
            file_id,
            file,
        }
    }

    pub fn sender(&self) -> &DeviceInfo {
        &self.sender
    }

    pub fn file_id(&self) -> &FileId {
        &self.file_id
    }

    pub fn file(&self) -> &FileMetadata {
        &self.file
    }
}

/// A protected prepare request.  The HTTP handler alone constructs it; a gate
/// receives the already-parsed credential and immutable single-file metadata.
pub struct CrossCopyAuthorizedPrepare {
    handoff: CrossCopyAuthorizedHandoff,
    metadata: CrossCopyAuthorizedPrepareMetadata,
}

impl CrossCopyAuthorizedPrepare {
    pub(crate) fn new(
        handoff: CrossCopyAuthorizedHandoff,
        metadata: CrossCopyAuthorizedPrepareMetadata,
    ) -> Self {
        Self { handoff, metadata }
    }

    pub fn metadata(&self) -> &CrossCopyAuthorizedPrepareMetadata {
        &self.metadata
    }

    pub fn into_parts(
        self,
    ) -> (
        CrossCopyAuthorizedHandoff,
        CrossCopyAuthorizedPrepareMetadata,
    ) {
        (self.handoff, self.metadata)
    }
}

impl std::fmt::Debug for CrossCopyAuthorizedPrepare {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CrossCopyAuthorizedPrepare")
            .field("handoff", &self.handoff)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// A one-shot stream of an HTTP body, represented without an Axum dependency.
/// Future safe-publication code consumes this incrementally; it must not buffer
/// the whole file in the LocalSend layer.
pub struct CrossCopyAuthorizedUploadBody {
    stream: Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>,
}

impl CrossCopyAuthorizedUploadBody {
    pub(crate) fn new(stream: Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>) -> Self {
        Self { stream }
    }

    pub async fn next_chunk(&mut self) -> Option<std::io::Result<Bytes>> {
        futures_util::StreamExt::next(&mut self.stream).await
    }
}

impl std::fmt::Debug for CrossCopyAuthorizedUploadBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CrossCopyAuthorizedUploadBody(..)")
    }
}

/// The protected upload handed to an opaque owner after its independently
/// minted LocalSend session token was verified.
pub struct CrossCopyAuthorizedUpload {
    session_id: SessionId,
    metadata: CrossCopyAuthorizedPrepareMetadata,
    body: CrossCopyAuthorizedUploadBody,
}

/// Opaque confirmation that the protected sink durably published its exact
/// file. The LocalSend handler emits its normal `FileReceived` event only
/// after this receipt returns; it never invents a path from protected session
/// metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossCopyAuthorizedUploadReceipt {
    path: PathBuf,
    size: u64,
}

impl CrossCopyAuthorizedUploadReceipt {
    pub fn new(path: PathBuf, size: u64) -> Self {
        Self { path, size }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

impl CrossCopyAuthorizedUpload {
    pub(crate) fn new(
        session_id: SessionId,
        metadata: CrossCopyAuthorizedPrepareMetadata,
        body: CrossCopyAuthorizedUploadBody,
    ) -> Self {
        Self {
            session_id,
            metadata,
            body,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn metadata(&self) -> &CrossCopyAuthorizedPrepareMetadata {
        &self.metadata
    }

    pub fn into_body(self) -> CrossCopyAuthorizedUploadBody {
        self.body
    }
}

impl std::fmt::Debug for CrossCopyAuthorizedUpload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CrossCopyAuthorizedUpload")
            .field("session_id", &self.session_id)
            .field("metadata", &self.metadata)
            .field("body", &self.body)
            .finish()
    }
}

/// The only capability the LocalSend handler receives after a successful
/// protected prepare.  It is one-shot by construction: `receive` consumes it,
/// and `cancel` consumes it on any terminal LocalSend cancellation/expiry.
#[async_trait]
pub trait CrossCopyAuthorizedUploadOwner: Send + Sync {
    /// Stays reachable while `receive` consumes this owner. HTTP cancel and
    /// orderly stop cancel this signal instead of losing all control once the
    /// request body starts streaming.
    fn cancellation(&self) -> CancellationToken;

    async fn receive(
        self: Box<Self>,
        upload: CrossCopyAuthorizedUpload,
    ) -> Result<CrossCopyAuthorizedUploadReceipt, CrossCopyAuthorizedUploadError>;

    async fn cancel(self: Box<Self>);
}

/// Narrow host-provided hook.  It atomically takes receiver-owned authority
/// before the LocalSend server creates a protected session or a per-file token.
#[async_trait]
pub trait CrossCopyAuthorizedUploadGate: Send + Sync {
    async fn take_authorized_upload(
        &self,
        prepare: CrossCopyAuthorizedPrepare,
    ) -> Result<Box<dyn CrossCopyAuthorizedUploadOwner>, CrossCopyAuthorizedUploadError>;
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CrossCopyAuthorizedUploadError {
    #[error("the CrossCopy File-v3 handoff header is missing, duplicated, or non-canonical")]
    InvalidHandoff,
    #[error("the CrossCopy File-v3 handoff was refused")]
    Refused,
    #[error("the CrossCopy File-v3 upload failed")]
    Failed,
}
