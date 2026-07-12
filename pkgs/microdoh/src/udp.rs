//! UDP listener — receives DNS queries from local clients and forwards them
//! to the curl worker thread via a channel.

use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::proto;

/// A task sent from the UDP listener to the curl worker.
pub struct DnsTask {
    /// Raw DNS wire-format query.
    pub query: Vec<u8>,
    /// Original DNS message ID (bytes 0-1) — must be restored in the response.
    /// RFC 8484 §4.1 requires ID=0 for GET, but the proxy must preserve the
    /// client's original ID.
    pub original_id: [u8; 2],
    /// Peer that sent the query (for the reply).
    #[allow(dead_code)]
    pub peer: SocketAddr,
    /// Channel to send the DNS response back.
    pub resp_tx: oneshot::Sender<Vec<u8>>,
}

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

        // Validate DNS query (minimum length, QR=0, QDCOUNT=1).
        if let Err(e) = proto::validate_dns_query(&buf[..n]) {
            log::debug!("dropping invalid DNS query from {peer}: {e}");
            continue;
        }

        // Save the original DNS ID before it gets zeroed for GET requests.
        let original_id = [buf[0], buf[1]];

        let (resp_tx, resp_rx) = oneshot::channel();
        let task = DnsTask {
            query: buf[..n].to_vec(),
            original_id,
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
                Ok(mut resp) => {
                    // Restore the original DNS ID in the response.
                    if resp.len() >= 2 {
                        resp[0] = original_id[0];
                        resp[1] = original_id[1];
                    }
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
