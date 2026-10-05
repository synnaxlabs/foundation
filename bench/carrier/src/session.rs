//! One session model over two carriers: QUIC (noq) and TLS 1.3 over TCP
//! (tokio-rustls). Both use rustls on aws-lc-rs with one cipher suite.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use noq::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use noq::{ConnectionError, MtuDiscoveryConfig, PathId, SendDatagramError, VarInt};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::Error;

/// The server name in the benchmark certificate.
const SERVER_NAME: &str = "carrier.test";
/// TLS bytes rustls holds beyond the socket. With `tcp_notsent_lowat`, this bounds
/// how long a frame waits behind bulk data on the sender.
const TLS_BUFFER: usize = 16 * 1024;
/// IPv4 and UDP headers.
const UDP_HEADERS: u16 = 28;
/// The smallest UDP payload QUIC allows.
const MIN_PAYLOAD: u16 = 1200;
/// QUIC flow control windows, above the bandwidth-delay product of a LAN.
const STREAM_WINDOW: u32 = 8 * 1024 * 1024;
const CONNECTION_WINDOW: u32 = 2 * STREAM_WINDOW;
const DATAGRAM_BUFFER: usize = 1024 * 1024;

/// The read half of one stream.
pub(crate) type Reader = Box<dyn AsyncRead + Unpin + Send>;
/// The write half of one stream.
pub(crate) type Writer = Box<dyn AsyncWrite + Unpin + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Carrier {
    Quic,
    /// One TCP connection with `TCP_NODELAY`; it carries one stream.
    Tls,
}

impl FromStr for Carrier {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "quic" => Ok(Self::Quic),
            "tls" => Ok(Self::Tls),
            _ => Err(format!("unknown carrier {s:?}").into()),
        }
    }
}

impl fmt::Display for Carrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Quic => "quic",
            Self::Tls => "tls",
        })
    }
}

/// The settings both ends of a session use.
pub(crate) struct Config {
    carrier: Carrier,
    provider: Arc<CryptoProvider>,
    unsegmented: bool,
    mtu: u16,
    transport: Arc<noq::TransportConfig>,
}

impl Config {
    /// `unsegmented` makes QUIC send one datagram per system call, without GSO. QUIC
    /// packets fill the link `mtu` from the first packet.
    ///
    /// # Errors
    ///
    /// When `mtu` leaves less than the QUIC minimum of 1200 bytes for a packet.
    pub(crate) fn new(
        carrier: Carrier,
        provider: Arc<CryptoProvider>,
        unsegmented: bool,
        mtu: u16,
    ) -> Result<Self, Error> {
        let payload = mtu
            .checked_sub(UDP_HEADERS)
            .filter(|&payload| payload >= MIN_PAYLOAD)
            .ok_or_else(|| {
                format!(
                    "MTU {mtu} is below the QUIC minimum of {}",
                    MIN_PAYLOAD + UDP_HEADERS
                )
            })?;
        let mut discovery = MtuDiscoveryConfig::default();
        discovery.upper_bound(payload);
        let mut transport = noq::TransportConfig::default();
        transport
            .enable_segmentation_offload(!unsegmented)
            .initial_mtu(payload)
            .mtu_discovery_config(Some(discovery))
            .congestion_controller_factory(Arc::new(
                noq::congestion::CubicConfig::default(),
            ))
            .stream_receive_window(VarInt::from_u32(STREAM_WINDOW))
            .receive_window(VarInt::from_u32(CONNECTION_WINDOW))
            .send_window(u64::from(CONNECTION_WINDOW))
            .datagram_receive_buffer_size(Some(DATAGRAM_BUFFER))
            .datagram_send_buffer_size(DATAGRAM_BUFFER);
        Ok(Self {
            carrier,
            provider,
            unsegmented,
            mtu,
            transport: Arc::new(transport),
        })
    }

    /// TLS 1.3 with AES-128-GCM only, which QUIC also needs for its Initial packets.
    pub(crate) fn provider() -> CryptoProvider {
        let mut provider = rustls::crypto::aws_lc_rs::default_provider();
        provider.cipher_suites =
            vec![rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256];
        provider
    }

    fn client_tls(&self, ca: &str) -> Result<rustls::ClientConfig, Error> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_file_iter(ca)? {
            roots.add(cert?)?;
        }
        Ok(
            rustls::ClientConfig::builder_with_provider(Arc::clone(&self.provider))
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    fn server_tls(&self, cert: &str, key: &str) -> Result<rustls::ServerConfig, Error> {
        let certs =
            CertificateDer::pem_file_iter(cert)?.collect::<Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_file(key)?;
        Ok(
            rustls::ServerConfig::builder_with_provider(Arc::clone(&self.provider))
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .with_no_client_auth()
                .with_single_cert(certs, key)?,
        )
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let suites: Vec<_> = self
            .provider
            .cipher_suites
            .iter()
            .map(rustls::SupportedCipherSuite::suite)
            .collect();
        let groups: Vec<_> = self.provider.kx_groups.iter().map(|g| g.name()).collect();
        write!(
            f,
            "carrier {}, unsegmented {}, MTU {}, suites {suites:?}, groups {groups:?}",
            self.carrier, self.unsegmented, self.mtu
        )?;
        if self.carrier == Carrier::Quic {
            write!(f, ", congestion Cubic, {:?}", self.transport)?;
        }
        Ok(())
    }
}

/// A connection to one peer. QUIC carries many streams and datagrams. TLS carries
/// one stream and no datagrams.
pub(crate) enum Session {
    Quic {
        conn: noq::Connection,
        endpoint: Option<noq::Endpoint>,
    },
    Tls {
        stream: Option<(Reader, Writer)>,
    },
}

impl Session {
    pub(crate) async fn connect(
        config: &Config,
        server: SocketAddr,
        ca: &str,
    ) -> Result<Self, Error> {
        let tls = config.client_tls(ca)?;
        match config.carrier {
            Carrier::Quic => {
                let endpoint =
                    noq::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;
                let mut client =
                    noq::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
                client.transport_config(Arc::clone(&config.transport));
                let conn = endpoint.connect_with(client, server, SERVER_NAME)?.await?;
                Ok(Self::Quic {
                    conn,
                    endpoint: Some(endpoint),
                })
            }
            Carrier::Tls => {
                let tcp = TcpStream::connect(server).await?;
                tcp.set_nodelay(true)?;
                let name = ServerName::try_from(SERVER_NAME)?;
                let mut stream =
                    TlsConnector::from(Arc::new(tls)).connect(name, tcp).await?;
                stream.get_mut().1.set_buffer_limit(Some(TLS_BUFFER));
                Ok(Self::tls(stream))
            }
        }
    }

    fn tls(stream: impl AsyncRead + AsyncWrite + Send + 'static) -> Self {
        let (r, w) = tokio::io::split(stream);
        Self::Tls {
            stream: Some((Box::new(r), Box::new(w))),
        }
    }

    pub(crate) fn carrier(&self) -> Carrier {
        match self {
            Self::Quic { .. } => Carrier::Quic,
            Self::Tls { .. } => Carrier::Tls,
        }
    }

    /// Opens a stream. A higher `priority` sends first.
    pub(crate) async fn open(
        &mut self,
        priority: i32,
    ) -> Result<(Reader, Writer), Error> {
        match self {
            Self::Quic { conn, .. } => {
                let (w, r) = conn.open_bi().await?;
                w.set_priority(priority)?;
                Ok((Box::new(r), Box::new(w)))
            }
            Self::Tls { stream } => stream
                .take()
                .ok_or_else(|| "a TLS session carries one stream".into()),
        }
    }

    /// The peer's next stream, or `None` once the peer closes the session.
    pub(crate) async fn accept(&mut self) -> Result<Option<(Reader, Writer)>, Error> {
        match self {
            Self::Quic { conn, .. } => match conn.accept_bi().await {
                Ok((w, r)) => Ok(Some((Box::new(r), Box::new(w)))),
                Err(e) if closed(&e) => Ok(None),
                Err(e) => Err(e.into()),
            },
            Self::Tls { stream } => Ok(stream.take()),
        }
    }

    /// The datagram side of a QUIC session.
    pub(crate) fn datagrams(&self) -> Option<Datagrams> {
        match self {
            Self::Quic { conn, .. } => Some(Datagrams(conn.clone())),
            Self::Tls { .. } => None,
        }
    }

    /// QUIC's own counters; `None` on TLS.
    pub(crate) fn stats(&self) -> Option<Stats> {
        let Self::Quic { conn, .. } = self else {
            return None;
        };
        let all = conn.stats();
        Some(Stats {
            lost_packets: all.lost_packets,
            sent: all.udp_tx.datagrams,
            received: all.udp_rx.datagrams,
            path: conn.path_stats(PathId::ZERO),
        })
    }

    /// Closes a client session and waits until the peer knows, so no server timer
    /// runs into the next test.
    pub(crate) async fn close(self) {
        if let Self::Quic {
            conn,
            endpoint: Some(endpoint),
        } = self
        {
            conn.close(VarInt::from_u32(0), b"done");
            endpoint.wait_idle().await;
        }
    }
}

/// Whether a session ended by a close, not by a failure.
fn closed(e: &ConnectionError) -> bool {
    matches!(
        e,
        ConnectionError::ApplicationClosed(_) | ConnectionError::LocallyClosed
    )
}

#[derive(Clone)]
pub(crate) struct Datagrams(noq::Connection);

impl Datagrams {
    pub(crate) fn send(&self, payload: Vec<u8>) -> Result<(), Error> {
        match self.0.send_datagram(payload.into()) {
            Err(SendDatagramError::ConnectionLost(e)) if closed(&e) => Ok(()),
            result => Ok(result?),
        }
    }

    /// The next datagram, or `None` once the session closes.
    pub(crate) async fn read(&self) -> Result<Option<Vec<u8>>, Error> {
        match self.0.read_datagram().await {
            Ok(datagram) => Ok(Some(datagram.to_vec())),
            Err(e) if closed(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

pub(crate) struct Stats {
    pub(crate) lost_packets: u64,
    sent: u64,
    received: u64,
    path: Option<noq::PathStats>,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lost packets {}, UDP datagrams sent {}, received {}",
            self.lost_packets, self.sent, self.received
        )?;
        if let Some(path) = &self.path {
            write!(
                f,
                ", RTT {:?}, cwnd {}, congestion events {}",
                path.rtt, path.cwnd, path.congestion_events
            )?;
        }
        Ok(())
    }
}

/// Accepts sessions on the server.
pub(crate) enum Listener {
    Quic(noq::Endpoint),
    Tls {
        listener: TcpListener,
        acceptor: TlsAcceptor,
    },
}

impl Listener {
    pub(crate) async fn bind(
        config: &Config,
        listen: SocketAddr,
        cert: &str,
        key: &str,
    ) -> Result<Self, Error> {
        let tls = config.server_tls(cert, key)?;
        match config.carrier {
            Carrier::Quic => {
                let mut server = noq::ServerConfig::with_crypto(Arc::new(
                    QuicServerConfig::try_from(tls)?,
                ));
                server.transport_config(Arc::clone(&config.transport));
                Ok(Self::Quic(noq::Endpoint::server(server, listen)?))
            }
            Carrier::Tls => Ok(Self::Tls {
                listener: TcpListener::bind(listen).await?,
                acceptor: TlsAcceptor::from(Arc::new(tls)),
            }),
        }
    }

    pub(crate) fn local_addr(&self) -> Result<SocketAddr, Error> {
        match self {
            Self::Quic(endpoint) => Ok(endpoint.local_addr()?),
            Self::Tls { listener, .. } => Ok(listener.local_addr()?),
        }
    }

    /// The next connection. Its handshake runs in [`Handshake::finish`], so a slow
    /// one does not hold up the next accept.
    pub(crate) async fn accept(&self) -> Result<Handshake, Error> {
        match self {
            Self::Quic(endpoint) => {
                let incoming = endpoint.accept().await.ok_or("the endpoint closed")?;
                Ok(Handshake::Quic(Box::new(incoming)))
            }
            Self::Tls { listener, acceptor } => {
                let (tcp, _) = listener.accept().await?;
                tcp.set_nodelay(true)?;
                Ok(Handshake::Tls(Box::new(acceptor.accept(tcp))))
            }
        }
    }
}

pub(crate) enum Handshake {
    Quic(Box<noq::Incoming>),
    Tls(Box<tokio_rustls::Accept<TcpStream>>),
}

impl Handshake {
    pub(crate) async fn finish(self) -> Result<Session, Error> {
        match self {
            Self::Quic(incoming) => Ok(Session::Quic {
                conn: (*incoming).await?,
                endpoint: None,
            }),
            Self::Tls(accept) => {
                let mut stream = accept.await?;
                stream.get_mut().1.set_buffer_limit(Some(TLS_BUFFER));
                Ok(Session::tls(stream))
            }
        }
    }
}
