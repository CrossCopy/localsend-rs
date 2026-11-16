//! The MCP surface: what an AI client can actually call.
//!
//! Eight tools and two resources, because that is the whole job. A model that
//! wants to move a file needs to (1) find the other machine, (2) hand it a path
//! it already knows, and (3) hear that it arrived. Everything else — pairing,
//! fingerprints, TLS, tokens — is the library's job and deliberately invisible
//! here; a tool whose argument is "the fingerprint you saw in `list_devices`"
//! would be honest and useless.
//!
//! # Receiving is where MCP is genuinely awkward
//!
//! A tool result is pull, not push: the protocol has no way to wake a model that
//! is not currently in a tool call. `notifications/resources/updated` is the
//! closest thing, but it is only defined for a client that subscribed, and most
//! clients do not. So the design is layered, and the docs say so out loud:
//!
//! * the default node **auto-accepts** every offer, so a file lands without a
//!   human, and the arrival is written to a durable journal ([`super::inbox`]);
//! * [`LocalSendMcp::wait_for_received`] is a long poll — the reliable path, and
//!   the one the instructions tell a model to use;
//! * `notifications/resources/updated` on `localsend://inbox` is a best-effort
//!   bonus for clients that *do* subscribe.
//!
//! That is the honest answer to "is notifying the AI worth it": yes for the
//! journal and the long poll, no for depending on notifications alone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents,
    ResourceUpdatedNotification, ResourceUpdatedNotificationParam, ServerCapabilities,
    ServerConfig, ServerNotification,
};
// The derive emits `schemars::…` paths, so the name has to resolve in this
// module; `rmcp` re-exports the exact version it expects, which keeps the two in
// lockstep instead of adding a second, possibly different, dependency.
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::serde_json;
use rmcp::service::NotificationContext;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler};
use serde::{Deserialize, Serialize};

use super::inbox::ReceivedFile;
use super::node::{Node, OfferSummary, SendReport};

/// The inbox, as a resource a client may read *and* subscribe to.
pub const INBOX_URI: &str = "localsend://inbox";

/// The device list, as a resource. Read-only, no subscription: devices come and
/// go constantly and a notification per arrival would be noise.
pub const DEVICES_URI: &str = "localsend://devices";

/// How many journalled entries a resource read returns. A model does not need a
/// thousand rows, and the journal keeps the rest.
const RESOURCE_ENTRIES: usize = 50;

/// The default long poll. Long enough to outlive one reasoning turn, short
/// enough that a client's own request timeout is unlikely to fire first.
const DEFAULT_WAIT: Duration = Duration::from_secs(30);

/// An upper bound on a single long poll, so a client cannot hold the connection
/// open indefinitely by asking for an hour.
const MAX_WAIT: Duration = Duration::from_secs(600);

/// One device, shaped for a model rather than for the wire.
#[derive(Clone, Debug, Serialize)]
pub struct DeviceView {
    /// What to pass as `device` when sending.
    pub alias: String,
    /// `address:port`, which `send_files` also accepts.
    pub address: String,
    pub protocol: String,
    pub model: String,
    pub kind: String,
    /// The peer's long-lived identity; stable across address changes.
    pub fingerprint: String,
    pub last_seen_seconds_ago: u64,
}

/// The argument to `list_devices`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ListDevicesArgs {
    /// Announce this machine and wait for answers before listing. Costs a second
    /// or two but finds devices that appeared since the last announcement.
    #[serde(default)]
    pub refresh: bool,
    /// How long to wait for answers when `refresh` is true. Default 2000 ms.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// The argument to `send_files`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct SendFilesArgs {
    /// A device from `list_devices`: its alias, its fingerprint, or its
    /// `address:port`. An address this process has never seen also works.
    pub device: String,
    /// Absolute paths to the files to send. Folders are refused; send their
    /// contents one by one.
    pub files: Vec<String>,
    /// The receiving device's PIN, when it uses one.
    #[serde(default)]
    pub pin: Option<String>,
}

/// The argument to `list_received`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ListReceivedArgs {
    /// How many entries, newest first. Default 20, capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The argument to `wait_for_received`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct WaitForReceivedArgs {
    /// The count from a previous call; only entries after it are returned. Omit
    /// to wait for anything at all, including entries already in the journal.
    #[serde(default)]
    pub cursor: Option<usize>,
    /// How long to wait. Default 30 seconds, capped at 600.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// The argument to `accept_offer` and `decline_offer`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct OfferArgs {
    /// The offer's `id`, from `list_offers`.
    pub id: u64,
}

/// What arrived, plus the cursor to pass to the next `wait_for_received`.
#[derive(Clone, Debug, Serialize)]
pub struct InboxPage {
    pub count: usize,
    pub cursor: usize,
    pub entries: Vec<ReceivedFile>,
}

/// The servers and tools an AI client sees.
#[derive(Clone)]
pub struct LocalSendMcp {
    node: Arc<Node>,
    tool_router: ToolRouter<Self>,
    /// One notification pump per process: `on_initialized` can fire again after a
    /// reconnect, and two pumps would double every notification.
    notifying: Arc<AtomicBool>,
}

#[tool_router]
impl LocalSendMcp {
    pub fn new(node: Arc<Node>) -> Self {
        Self {
            node,
            tool_router: Self::tool_router(),
            notifying: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Find the other machines.
    #[tool(
        description = "List the LocalSend devices on this local network. Call this first, \
                       then pass one device's alias (or address:port) to send_files. \
                       Set refresh=true to announce and wait for fresh answers."
    )]
    async fn list_devices(
        &self,
        Parameters(args): Parameters<ListDevicesArgs>,
    ) -> Result<String, String> {
        let peers = if args.refresh {
            let wait = args
                .wait_ms
                .map(|ms| Duration::from_millis(ms.clamp(100, 15_000)))
                .unwrap_or(Duration::from_millis(2_000));
            self.node.discover(wait).await
        } else {
            self.node.devices()
        };

        let devices = device_views(&peers);

        if devices.is_empty() {
            return Ok(format!(
                "No devices answered. This machine (pid {}) is listening on port {} as `{}`; \
                 make sure the other computer has LocalSend open on the same network, then call \
                 list_devices again with refresh=true. An `address:port` target also works \
                 without discovery.",
                self.node.status().pid,
                self.node.status().port,
                self.node.status().alias,
            ));
        }
        render(&devices)
    }

    /// Move files to a peer.
    #[tool(
        description = "Send one or more files from this machine to another LocalSend device. \
                       The device may be an alias from list_devices, an address like \
                       `192.168.1.5:53317`, or a fingerprint. Returns one report naming every \
                       file and, if something failed, the reason."
    )]
    async fn send_files(
        &self,
        Parameters(args): Parameters<SendFilesArgs>,
    ) -> Result<String, String> {
        let peer = self.node.resolve(&args.device).await?;
        let paths: Vec<std::path::PathBuf> =
            args.files.iter().map(std::path::PathBuf::from).collect();
        let report: SendReport = self.node.send(&peer, &paths, args.pin).await;
        if let Some(error) = &report.error {
            return Err(format!(
                "{error}\n\n{} of {} file(s) had not been confirmed when the transfer stopped.",
                report.files.len(),
                paths.len()
            ));
        }
        Ok(format!(
            "Sent {} file(s), {} bytes, to `{}` over {} at {}.\n{}",
            report.files.len(),
            report.bytes,
            report.device,
            report.protocol,
            report.endpoint,
            render(&report)?
        ))
    }

    /// Read the journal.
    #[tool(
        description = "List files and text messages this MCP node has received, newest first. \
                       The inbox is a journal on disk, so it survives the process: entries from \
                       earlier sessions are here too."
    )]
    async fn list_received(
        &self,
        Parameters(args): Parameters<ListReceivedArgs>,
    ) -> Result<String, String> {
        let limit = args.limit.unwrap_or(20).clamp(1, 1000);
        let entries = self.node.received(limit);
        let page = InboxPage {
            count: entries.len(),
            cursor: self.node.received_count(),
            entries,
        };
        if page.count == 0 {
            return Ok(format!(
                "Nothing has been received yet (cursor {}). Call wait_for_received to wait for \
                 the next arrival.",
                page.cursor
            ));
        }
        render(&page)
    }

    /// The reliable notification path.
    #[tool(
        description = "Wait until a file or text message is received, then return it. This is \
                       how to be notified of an incoming transfer: pass the `cursor` from the \
                       previous response, and call it again with the new cursor to keep waiting. \
                       Returns an empty list when the timeout passes."
    )]
    async fn wait_for_received(
        &self,
        Parameters(args): Parameters<WaitForReceivedArgs>,
    ) -> Result<String, String> {
        let cursor = args.cursor.unwrap_or(0);
        let timeout = args
            .timeout_seconds
            .map(|seconds| Duration::from_secs(seconds.clamp(1, MAX_WAIT.as_secs())))
            .unwrap_or(DEFAULT_WAIT);
        let entries = self.node.wait_for_new(cursor, timeout).await;
        let page = InboxPage {
            count: entries.len(),
            cursor: self.node.received_count(),
            entries,
        };
        if page.count == 0 {
            return Ok(format!(
                "Nothing arrived within {}s. Call wait_for_received again with cursor {} to keep \
                 waiting — the receiver is still listening on port {}.",
                timeout.as_secs(),
                page.cursor,
                self.node.status().port
            ));
        }
        render(&page)
    }

    /// Offers, in `--ask` mode.
    #[tool(
        description = "List transfer offers waiting for an answer. Only relevant when the server \
                       was started with --ask; in the default auto-accept mode a transfer is \
                       accepted on arrival and never appears here. Offers expire after five \
                       minutes, which declines them."
    )]
    async fn list_offers(&self) -> Result<String, String> {
        let offers: Vec<OfferSummary> = self.node.offers();
        if offers.is_empty() {
            return Ok(if self.node.status().auto_accept {
                "No offers are waiting: this node auto-accepts, so files land directly in the \
                 inbox. Use list_received."
                    .to_string()
            } else {
                "No offers are waiting. A sender that is waiting for an answer will show up here \
                 for five minutes."
                    .to_string()
            });
        }
        render(&offers)
    }

    /// Take an offer.
    #[tool(
        description = "Accept a waiting offer by id, so the sender uploads its files. Use \
                       list_offers first to see the ids."
    )]
    async fn accept_offer(
        &self,
        Parameters(args): Parameters<OfferArgs>,
    ) -> Result<String, String> {
        match self.node.accept_offer(args.id) {
            Some(summary) => {
                Ok(format!(
                "Accepted offer {} from `{}` ({} file(s), {} bytes). The sender is uploading now; \
                 call list_received or wait_for_received to see them arrive.",
                summary.id, summary.from, summary.files.len(), summary.bytes
            ))
            }
            None => Err(format!(
                "No offer {} is waiting. It may have expired or been answered already — call \
                 list_offers.",
                args.id
            )),
        }
    }

    /// Refuse an offer.
    #[tool(
        description = "Decline a waiting offer by id, which tells the sender immediately instead \
                       of making it wait out the five-minute timeout."
    )]
    async fn decline_offer(
        &self,
        Parameters(args): Parameters<OfferArgs>,
    ) -> Result<String, String> {
        match self.node.decline_offer(args.id) {
            Some(summary) => Ok(format!(
                "Declined offer {} from `{}`. The sender has been told.",
                summary.id, summary.from
            )),
            None => Err(format!(
                "No offer {} is waiting. It may have expired or been answered already — call \
                 list_offers.",
                args.id
            )),
        }
    }

    /// What this process is.
    #[tool(
        description = "Report this MCP node's identity: alias, port, protocol, fingerprint, \
                       receive folder, journal path and counters. Useful to confirm the receiver \
                       is listening or to tell another machine where to send."
    )]
    async fn server_status(&self) -> Result<String, String> {
        render(&self.node.status())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LocalSendMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("localsend", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "LocalSend is a peer-to-peer file transfer protocol for the local network. This \
             server runs one LocalSend node for this session, as `<your device name> (MCP)`.\n\n\
             Sending: call list_devices to see the other machines (or use an address:port you \
             were given), then send_files with absolute paths on this machine. One call may carry \
             several files; the report names each one.\n\n\
             Receiving: files addressed to this node are accepted automatically and written to \
             the receive folder shown by server_status. Every arrival is appended to a journal on \
             disk, so list_received still answers after a restart. To be told about an arrival, \
             call wait_for_received with the cursor from the previous response: it blocks until \
             something lands or the timeout passes. MCP has no push notification to a model that \
             is idle, so this long poll is the reliable path; a resources/updated notification on \
             localsend://inbox is only delivered to clients that subscribe.\n\n\
             If the server was started with --ask, offers wait five minutes for accept_offer or \
             decline_offer, and list_offers shows them. With --no-receive there is no receiver at \
             all and only sending works.\n\n\
             Sending is irreversible: the file leaves this machine. Do not send anything the user \
             has not asked for, and do not invent paths — use the ones the user gave you."
                .to_string(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(INBOX_URI, "LocalSend inbox")
                .with_description(
                    "Files and text messages received by this node, newest first, from the durable \
                     journal.",
                )
                .with_mime_type("application/json"),
            Resource::new(DEVICES_URI, "LocalSend devices")
                .with_description("The LocalSend devices this node has seen on the local network.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        match request.uri.as_str() {
            INBOX_URI => {
                let page = InboxPage {
                    count: self.node.received(RESOURCE_ENTRIES).len(),
                    cursor: self.node.received_count(),
                    entries: self.node.received(RESOURCE_ENTRIES),
                };
                Ok(ReadResourceResult::new(vec![contents(INBOX_URI, &page)?]).into())
            }
            DEVICES_URI => {
                let devices = device_views(&self.node.devices());
                Ok(ReadResourceResult::new(vec![contents(DEVICES_URI, &devices)?]).into())
            }
            other => Err(rmcp::ErrorData::resource_not_found(
                format!("`{other}` is not a resource of this server"),
                Some(serde_json::json!({ "uri": other })),
            )),
        }
    }

    /// Best-effort push: only reaches a client that subscribed to the resource.
    /// `wait_for_received` is the path that always works, which is what the
    /// instructions say — see the module comment.
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if self.notifying.swap(true, Ordering::SeqCst) {
            return;
        }
        let node = self.node.clone();
        let peer = context.peer;
        tokio::spawn(async move {
            let signal = node.file_signal();
            let mut seen = node.received_count();
            loop {
                // Subscribe before reading the counter, so a file that lands
                // in between cannot be swallowed. `Notified` only registers
                // when polled, so the counter is checked again after every
                // wake-up and on the tick — a missed pulse costs seconds,
                // never an entry.
                let notified = signal.notified();
                let count = node.received_count();
                if count > seen {
                    seen = count;
                    let notification = ServerNotification::ResourceUpdatedNotification(
                        ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(
                            INBOX_URI,
                        )),
                    );
                    if peer.send_notification(notification).await.is_err() {
                        // The client is gone; this task's lifetime is the
                        // session's, so stop rather than spin.
                        return;
                    }
                    continue;
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), notified).await;
            }
        });
    }
}

/// The device list in the shape a model reads: what to call it, where it is, and
/// how long ago it was last seen.
fn device_views(peers: &[crate::service::Peer]) -> Vec<DeviceView> {
    let now = std::time::SystemTime::now();
    peers
        .iter()
        .map(|peer| DeviceView {
            alias: peer.alias.clone(),
            address: format!("{}:{}", peer.ip, peer.port),
            protocol: peer.protocol.as_str().to_uppercase(),
            model: peer.model.clone(),
            kind: match peer.device_kind {
                crate::service::PeerKind::Phone => "phone",
                crate::service::PeerKind::Computer => "computer",
                crate::service::PeerKind::Server => "server",
            }
            .to_string(),
            fingerprint: peer.node_id.clone(),
            last_seen_seconds_ago: now
                .duration_since(peer.last_seen)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
        })
        .collect()
}

/// JSON, indented: a model reads it either way, and a human debugging a client
/// can too.
fn render<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value)
        .map_err(|error| format!("could not render the result: {error}"))
}

fn contents<T: Serialize>(uri: &str, value: &T) -> Result<ResourceContents, rmcp::ErrorData> {
    let text = serde_json::to_string_pretty(value).map_err(|error| {
        rmcp::ErrorData::internal_error(format!("could not render {uri}: {error}"), None)
    })?;
    Ok(ResourceContents::text(text, uri).with_mime_type("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn the_schema_of_every_tool_argument_is_an_object_with_described_fields() {
        // A tool whose schema is not an object is a tool a client cannot fill
        // in; the macro builds these, so the check is that the arguments are
        // shaped the way the model sees them.
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(SendFilesArgs)).unwrap();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["device"].is_object());
        assert!(schema["properties"]["files"].is_object());
        assert_eq!(
            schema["required"],
            serde_json::json!(["device", "files"]),
            "a send without a device or files must be refused by the schema, not by us"
        );

        let schema = serde_json::to_value(rmcp::schemars::schema_for!(ListDevicesArgs)).unwrap();
        assert_eq!(
            schema.get("required"),
            None,
            "every list_devices field has a default, so a bare `{{}}` call is valid"
        );
    }

    #[::core::prelude::v1::test]
    fn an_omitted_cursor_means_nothing_has_been_read_yet() {
        let args: WaitForReceivedArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(args.cursor, None);
        assert_eq!(args.timeout_seconds, None);
        // The struct's own default has to mean the same thing as a missing field.
        assert_eq!(WaitForReceivedArgs::default().cursor, None);
    }

    #[::core::prelude::v1::test]
    fn a_resource_read_of_the_inbox_carries_the_cursor_a_client_needs() {
        let page = InboxPage {
            count: 0,
            cursor: 7,
            entries: Vec::new(),
        };
        let text = contents(INBOX_URI, &page).unwrap();
        let ResourceContents::TextResourceContents {
            text,
            mime_type,
            uri,
            ..
        } = text
        else {
            panic!("the inbox is text, not a blob");
        };
        assert_eq!(uri, INBOX_URI);
        assert_eq!(mime_type.as_deref(), Some("application/json"));
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["cursor"], 7);
    }
}
