use super::Result;
use crate::LocalSendError;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;

#[derive(Default)]
pub(super) struct AnnouncementSendSummary {
    successful_sends: usize,
    failed_sends: usize,
    first_failure: Option<String>,
}

impl AnnouncementSendSummary {
    pub(super) fn record(&mut self, result: std::io::Result<usize>) {
        match result {
            Ok(_) => self.successful_sends += 1,
            Err(error) if self.first_failure.is_none() => {
                self.failed_sends += 1;
                self.first_failure = Some(error.to_string());
            }
            Err(_) => self.failed_sends += 1,
        }
    }

    pub(super) fn finish(self) -> Result<()> {
        if self.successful_sends > 0 {
            if self.failed_sends > 0 {
                tracing::warn!(
                    successful_sends = self.successful_sends,
                    failed_sends = self.failed_sends,
                    first_error = self.first_failure.as_deref().unwrap_or("unknown error"),
                    "LocalSend multicast announcement was unavailable on some interfaces"
                );
            }
            return Ok(());
        }

        Err(LocalSendError::network(format!(
            "Failed to announce presence on every interface: {}",
            self.first_failure
                .as_deref()
                .unwrap_or("no interface accepted the datagram")
        )))
    }

    pub(super) fn needs_recovery_retry(&self) -> bool {
        self.successful_sends == 0
    }
}

pub(super) async fn send_announcement_round(
    sockets: &[Arc<UdpSocket>],
    buf: &[u8],
    multicast_addr: SocketAddr,
    attempt: usize,
    summary: &mut AnnouncementSendSummary,
) {
    for (socket_index, socket) in sockets.iter().enumerate() {
        let result = socket.send_to(buf, &multicast_addr).await;
        if let Err(error) = &result {
            tracing::debug!(
                attempt,
                socket_index,
                %error,
                "LocalSend multicast announcement failed on one socket"
            );
        }
        summary.record(result);
    }
}
