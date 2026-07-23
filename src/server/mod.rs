#![allow(clippy::module_inception)]

pub mod crosscopy_authorized;
pub mod events;
pub mod server;
pub mod web_share;

pub(crate) mod handlers;
pub(crate) mod pin;
pub(crate) mod routes;
pub(crate) mod state;

pub use crosscopy_authorized::{
    CROSSCOPY_FILE_V3_HANDOFF_HEADER, CrossCopyAuthorizedHandoff, CrossCopyAuthorizedPrepare,
    CrossCopyAuthorizedPrepareMetadata, CrossCopyAuthorizedUpload, CrossCopyAuthorizedUploadBody,
    CrossCopyAuthorizedUploadError, CrossCopyAuthorizedUploadGate, CrossCopyAuthorizedUploadOwner,
    CrossCopyAuthorizedUploadReceipt,
};
pub use events::{PendingRequest, PendingWebShareRequest, ServerEvent, TransferDecision};
pub use server::{LocalSendServer, LocalSendServerBuilder};
pub use web_share::{WebShareFile, WebShareSource};
