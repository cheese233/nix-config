//! UDP listener — receives DNS queries from local clients and forwards them
//! to the curl worker thread via a channel.

use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

/// A task sent from the UDP listener to the curl worker.
pub struct DnsTask {
    /// Raw DNS wire-format query.
    pub query: Vec<u8>,
    /// Peer that sent the query (for the reply).
    #[allow(dead_code)]
    pub peer: SocketAddr,
    /// Channel to send the DNS response back.
    pub resp_tx: oneshot::Sender<Vec<u8>>,
}

/// Minimum DNS message size (RFC 1035 §4.2.1: header is 12 bytes).
const MIN_DNS_LEN: usize = 12;

/// Listen on `addr` for UDP DNS queries and forward them through `tx`.
pub async fn udp_loop(addr: &str, tx: mpsc::UnboundedSender<DnsTask>) -> anyhow::Result<()> {
    let sock = std::sync::Arc::new(UdpSocket::bind(addr).await?);
    log::info!("listening on udp://{addr}");

    let mut buf = vec![0u8; 4096]; // EDNS0 can be up to 4096

    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("recv_from: {e}");
                continue;
            }
        };

        if n < MIN_DNS_LEN {
            log::debug!("dropping short packet ({n} bytes) from {peer}");
            continue;
        }

        let (resp_tx, resp_rx) = oneshot::channel();
        let task = DnsTask {
            query: buf[..n].to_vec(),
            peer,
            resp_tx,
        };

        if tx.send(task).is_err() {
            log::debug!("curl worker channel closed, dropping query from {peer}");
            continue;
        }

        // Spawn a task to await the response and send it back over UDP.
        let sock = sock.clone();
        tokio::spawn(async move {
            match resp_rx.await {
                Ok(resp) => {
                    if let Err(e) = sock.send_to(&resp, peer).await {
                        log::warn!("send_to {peer}: {e}");
                    }
                }
                Err(_) => {
                    log::debug!("response channel dropped for {peer}");
                }
            }
        });
    }
}
