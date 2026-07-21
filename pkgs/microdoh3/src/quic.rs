//! noq-proto (sans-io QUIC) driver — no async runtime.
//!
//! Owns the proto `Endpoint` and at most one client `Connection`. The worker
//! owns the OS socket and event loop; this module translates between UDP
//! datagrams and QUIC state-machine calls, with GRO/GSO batching via noq-udp.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use noq_proto as proto;
use noq_proto::crypto::rustls::QuicClientConfig;
use noq_udp::{RecvMeta, UdpSockRef, UdpSocketState, BATCH_SIZE};

/// ALPN for HTTP/3.
pub const ALPN_H3: &[u8] = b"h3";

/// One GRO-capable receive buffer slot size (max UDP payload).
const IOV_SIZE: usize = 65536;

#[derive(Debug, thiserror::Error)]
pub enum QuicError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("connect: {0}")]
    Connect(#[from] proto::ConnectError),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("crypto: {0}")]
    Crypto(#[from] proto::crypto::rustls::NoInitialCipherSuite),
    #[error("no connection")]
    NoConn,
    #[error("write: {0}")]
    Write(#[from] proto::WriteError),
    #[error("finish: {0}")]
    Finish(#[from] proto::FinishError),
}

pub struct Quic {
    endpoint: proto::Endpoint,
    conn: Option<proto::Connection>,
    handle: Option<proto::ConnectionHandle>,
    client_config: proto::ClientConfig,
    udp_state: UdpSocketState,
    server_name: String,
    /// Preallocated scratch for `poll_transmit`.
    send_scratch: Vec<u8>,
    /// Preallocated receive arena (BATCH_SIZE × IOV_SIZE).
    recv_arena: Vec<u8>,
    /// Scratch for endpoint-generated immediate responses.
    response_scratch: Vec<u8>,
    /// Stats for logging.
    pub zero_rtt_accepted: bool,
}

/// Build the QUIC client config: webpki roots, ALPN h3, TLS 1.3 early data.
pub fn build_client_config(keep_alive: Duration, idle: Duration) -> Result<proto::ClientConfig, QuicError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN_H3.to_vec()];
    crypto.enable_early_data = true;
    // rustls keeps an in-memory session cache (256 entries) by default → 0-RTT
    // on reconnects within this process.

    let quic_crypto = QuicClientConfig::try_from(crypto)?;
    let mut cfg = proto::ClientConfig::new(Arc::new(quic_crypto));

    let mut transport = proto::TransportConfig::default();
    transport.keep_alive_interval(Some(keep_alive));
    if let Ok(t) = proto::IdleTimeout::try_from(idle) {
        transport.max_idle_timeout(Some(t));
    }
    // BBRv3 congestion control (model-based pacing; better than CUBIC on
    // lossy/high-BDP paths to the DoH upstream).
    transport.congestion_controller_factory(Arc::new(proto::congestion::Bbr3Config::default()));
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
}

impl Quic {
    pub fn new(
        client_config: proto::ClientConfig,
        server_name: String,
        udp_state: UdpSocketState,
    ) -> Self {
        let endpoint = proto::Endpoint::new(Arc::new(proto::EndpointConfig::default()), None, true);
        Self {
            endpoint,
            conn: None,
            handle: None,
            client_config,
            udp_state,
            server_name,
            send_scratch: Vec::with_capacity(64 * 1024),
            recv_arena: vec![0; BATCH_SIZE * IOV_SIZE],
            response_scratch: Vec::with_capacity(1500),
            zero_rtt_accepted: false,
        }
    }

    /// Initialize the UDP socket state (enables GRO, PMTUD sockopts).
    pub fn init_socket(sock: &UdpSocket) -> io::Result<UdpSocketState> {
        UdpSocketState::new(UdpSockRef::from(sock))
    }

    /// Start a new connection to `remote`. Any previous connection is dropped.
    pub fn connect(&mut self, now: Instant, remote: SocketAddr, sock: &UdpSocket) -> Result<(), QuicError> {
        let (handle, conn) = self.endpoint.connect(
            now,
            self.client_config.clone(),
            remote,
            &self.server_name,
        )?;
        log::info!(
            "quic: connecting to {remote} (server_name={}, 0-rtt={})",
            self.server_name,
            conn.has_0rtt()
        );
        self.conn = Some(conn);
        self.handle = Some(handle);
        self.zero_rtt_accepted = false;
        self.flush(now, sock)?;
        Ok(())
    }

    pub fn has_conn(&self) -> bool {
        self.conn.is_some()
    }

    /// Whether 0-RTT data is possible on this connection attempt
    /// (i.e. remembered transport parameters allow opening streams
    /// before the handshake completes).
    pub fn has_0rtt(&self) -> bool {
        self.conn.as_ref().is_some_and(|c| c.has_0rtt())
    }

    pub fn is_drained(&self) -> bool {
        self.conn.as_ref().is_none_or(|c| c.is_drained())
    }

    /// Drop the current connection (after loss/GOAWAY drain).
    pub fn drop_conn(&mut self) {
        self.conn = None;
        self.handle = None;
    }

    /// Open a new bidirectional stream for a request.
    pub fn open_bi(&mut self) -> Option<proto::StreamId> {
        self.conn.as_mut()?.streams().open(proto::Dir::Bi)
    }

    /// Open the client control stream (first client uni stream).
    pub fn open_control_uni(&mut self) -> Option<proto::StreamId> {
        self.conn.as_mut()?.streams().open(proto::Dir::Uni)
    }

    /// Write all of `data` to a stream (requests are small; never blocks).
    pub fn write_all(&mut self, id: proto::StreamId, data: &[u8]) -> Result<(), QuicError> {
        let conn = self.conn.as_mut().ok_or(QuicError::NoConn)?;
        let mut off = 0;
        while off < data.len() {
            off += conn.send_stream(id).write(&data[off..])?;
        }
        Ok(())
    }

    pub fn finish_stream(&mut self, id: proto::StreamId) -> Result<(), QuicError> {
        let conn = self.conn.as_mut().ok_or(QuicError::NoConn)?;
        conn.send_stream(id).finish()?;
        Ok(())
    }

    /// Read available data from a stream; calls `f` with each chunk.
    /// Returns (eof, error_encountered).
    pub fn read_stream(
        &mut self,
        id: proto::StreamId,
        mut f: impl FnMut(&[u8]),
    ) -> (bool, bool) {
        let Some(conn) = self.conn.as_mut() else {
            return (false, true);
        };
        let mut stream = conn.recv_stream(id);
        let mut chunks = match stream.read(true) {
            Ok(c) => c,
            Err(proto::ReadableError::ClosedStream) => return (true, false),
            Err(_) => return (false, true),
        };
        let mut eof = false;
        loop {
            match chunks.next(usize::MAX) {
                Ok(Some(chunk)) => f(&chunk.bytes),
                Ok(None) => {
                    eof = true;
                    break;
                }
                Err(proto::ReadError::Blocked) => break,
                Err(_) => return (false, true),
            }
        }
        let _ = chunks.finalize();
        (eof, false)
    }

    /// Process all readable UDP datagrams on the QUIC socket.
    pub fn poll_socket(&mut self, now: Instant, sock: &UdpSocket) -> io::Result<()> {
        // Small fixed queue of received datagrams per syscall batch.
        let mut datagrams: Vec<(RecvMeta, BytesMut)> = Vec::with_capacity(BATCH_SIZE * 4);
        loop {
            let mut metas = [RecvMeta::default(); BATCH_SIZE];
            let n = {
                let mut iovs: Vec<std::io::IoSliceMut<'_>> = self
                    .recv_arena
                    .chunks_mut(IOV_SIZE)
                    .map(std::io::IoSliceMut::new)
                    .collect();
                match self
                    .udp_state
                    .recv(UdpSockRef::from(sock), &mut iovs, &mut metas)
                {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => 0,
                    Err(e) => return Err(e),
                }
            };
            if n == 0 {
                if datagrams.is_empty() {
                    return Ok(());
                }
            } else {
                for (i, meta) in metas.iter().take(n).enumerate() {
                    let data = &self.recv_arena[i * IOV_SIZE..i * IOV_SIZE + meta.len];
                    let stride = if meta.stride == 0 {
                        data.len().max(1)
                    } else {
                        meta.stride
                    };
                    for seg in data.chunks(stride) {
                        datagrams.push((*meta, BytesMut::from(seg)));
                    }
                }
            }
            // Process everything received so far, then loop for more.
            for (meta, data) in datagrams.drain(..) {
                self.response_scratch.clear();
                let event = self.endpoint.handle(
                    now,
                    proto::FourTuple::new(meta.addr, meta.dst_ip),
                    meta.ecn.map(ecn_to_proto),
                    data,
                    &mut self.response_scratch,
                );
                self.handle_datagram_event(now, event, sock);
            }
            self.drain_endpoint_events();
            if n == 0 {
                return Ok(());
            }
        }
    }

    fn handle_datagram_event(
        &mut self,
        now: Instant,
        event: Option<proto::DatagramEvent>,
        sock: &UdpSocket,
    ) {
        match event {
            Some(proto::DatagramEvent::ConnectionEvent(h, ev)) => {
                if Some(h) == self.handle {
                    if let Some(conn) = self.conn.as_mut() {
                        conn.handle_event(ev);
                    }
                }
            }
            Some(proto::DatagramEvent::Response(transmit)) => {
                let _ = self.send_transmit(&transmit, sock);
            }
            Some(proto::DatagramEvent::NewConnection(_)) => {
                log::trace!("quic: ignoring incoming connection attempt (client mode)");
            }
            None => {}
        }
        let _ = now;
    }

    /// Transfer pending endpoint events from the connection back to the endpoint.
    fn drain_endpoint_events(&mut self) {
        let (Some(conn), Some(handle)) = (self.conn.as_mut(), self.handle) else {
            return;
        };
        while let Some(ev) = conn.poll_endpoint_events() {
            if let Some(cev) = self.endpoint.handle_event(handle, ev) {
                conn.handle_event(cev);
            }
        }
    }

    /// Drain application-level events.
    pub fn poll_events(&mut self) -> Vec<proto::Event> {
        let mut out = Vec::new();
        if let Some(conn) = self.conn.as_mut() {
            while let Some(ev) = conn.poll() {
                if matches!(ev, proto::Event::Connected) && conn.accepted_0rtt() {
                    self.zero_rtt_accepted = true;
                    log::info!("quic: 0-RTT accepted by server");
                }
                out.push(ev);
            }
        }
        out
    }

    /// Send everything the connection wants to send.
    pub fn flush(&mut self, now: Instant, sock: &UdpSocket) -> io::Result<()> {
        let Some(conn) = self.conn.as_mut() else {
            return Ok(());
        };
        let max = self.udp_state.max_gso_segments();
        loop {
            self.send_scratch.clear();
            let Some(t) = conn.poll_transmit(now, max, &mut self.send_scratch) else {
                break;
            };
            let ut = noq_udp::Transmit {
                destination: t.destination,
                ecn: t.ecn.map(ecn_to_udp),
                contents: &self.send_scratch[..t.size],
                segment_size: t.segment_size,
                src_ip: t.src_ip,
            };
            self.udp_state.send(UdpSockRef::from(sock), &ut)?;
        }
        Ok(())
    }

    /// Send an endpoint-generated response transmit.
    fn send_transmit(&mut self, t: &proto::Transmit, sock: &UdpSocket) -> io::Result<()> {
        let ut = noq_udp::Transmit {
            destination: t.destination,
            ecn: t.ecn.map(ecn_to_udp),
            contents: &self.response_scratch[..t.size],
            segment_size: t.segment_size,
            src_ip: t.src_ip,
        };
        self.udp_state.send(UdpSockRef::from(sock), &ut)
    }

    /// Next QUIC timer deadline, if any.
    pub fn next_timeout(&mut self) -> Option<Instant> {
        self.conn.as_mut().and_then(|c| c.poll_timeout())
    }

    /// Fire due QUIC timers.
    pub fn handle_timeout(&mut self, now: Instant, sock: &UdpSocket) -> io::Result<()> {
        if let Some(conn) = self.conn.as_mut() {
            conn.handle_timeout(now);
        }
        self.drain_endpoint_events();
        self.flush(now, sock)
    }

    /// Gracefully close the connection.
    pub fn close(&mut self, now: Instant, sock: &UdpSocket) {
        if let Some(conn) = self.conn.as_mut() {
            conn.close(now, 0u32.into(), bytes::Bytes::from_static(b"shutdown"));
        }
        let _ = self.flush(now, sock);
    }
}

fn ecn_to_proto(e: noq_udp::EcnCodepoint) -> proto::EcnCodepoint {
    match e {
        noq_udp::EcnCodepoint::Ect0 => proto::EcnCodepoint::Ect0,
        noq_udp::EcnCodepoint::Ect1 => proto::EcnCodepoint::Ect1,
        noq_udp::EcnCodepoint::Ce => proto::EcnCodepoint::Ce,
    }
}

fn ecn_to_udp(e: proto::EcnCodepoint) -> noq_udp::EcnCodepoint {
    match e {
        proto::EcnCodepoint::Ect0 => noq_udp::EcnCodepoint::Ect0,
        proto::EcnCodepoint::Ect1 => noq_udp::EcnCodepoint::Ect1,
        proto::EcnCodepoint::Ce => noq_udp::EcnCodepoint::Ce,
    }
}
