#![allow(clippy::module_inception)]

pub mod crosscopy_authorized;
pub mod events;
pub mod server;
pub mod web_share;

pub(crate) mod handlers;
/// Receiver-side PIN enforcement.
///
/// **Public because a consumer that serves the v2 routes from its own router
/// has to enforce the same rule.** The gate is behaviour the protocol
/// specifies — 401 on a mismatch, three failures then 429 for five minutes,
/// per peer — and a consumer left to write its own would write a different
/// one. The same reasoning that made `web_share_router` public.
pub mod pin;
pub(crate) mod routes;
pub(crate) mod state;

pub use crosscopy_authorized::{
    CROSSCOPY_FILE_V3_HANDOFF_HEADER, CrossCopyAuthorizedHandoff, CrossCopyAuthorizedPrepare,
    CrossCopyAuthorizedPrepareMetadata, CrossCopyAuthorizedUpload, CrossCopyAuthorizedUploadBody,
    CrossCopyAuthorizedUploadError, CrossCopyAuthorizedUploadGate, CrossCopyAuthorizedUploadOwner,
    CrossCopyAuthorizedUploadReceipt,
};
pub use events::{PendingRequest, PendingWebShareRequest, ServerEvent, TransferDecision};
pub use pin::{GUESS_PRESSURE, LOCKOUT, MAX_FAILURES, MAX_TRACKED_PEERS, PinGate, PinVerdict};
pub use server::{LocalSendServer, LocalSendServerBuilder};
pub use web_share::{WebShareFile, WebShareHost, WebShareSource, WebShareState, web_share_router};
