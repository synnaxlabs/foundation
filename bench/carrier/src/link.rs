//! Sets up each carrier: TLS and QUIC configs, the client link, and the server.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use noq::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::{Error, SERVER_NAME, test};

/// The read half of one stream.
pub type Reader = Box<dyn AsyncRead + Unpin + Send>;
/// The write half of one stream.
pub type Writer = Box<dyn AsyncWrite + Unpin + Send>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    /// QUIC; latency frames go on a stream.
    Quic,
    /// QUIC; latency frames go in datagrams.
    QuicDatagram,
    /// TLS 1.3 over TCP with `TCP_NODELAY`; each stream is its own connection.
    Tls,
}

impl FromStr for Carrier {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "quic" => Ok(Self::Quic),
            "quic-dgram" => Ok(Self::QuicDatagram),
            "tls" => Ok(Self::Tls),
            _ => Err(format!("unknown carrier {s:?}").into()),
        }
    }
}

impl fmt::Display for Carrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Quic => "quic",
            Self::QuicDatagram => "quic-dgram",
            Self::Tls => "tls",
        })
    }
}

/// A client's way to open streams to the server.
pub struct Link {
    pub carrier: Carrier,
    pub gso: bool,
    kind: Kind,
}

enum Kind {
    Quic {
        conn: noq::Connection,
        _endpoint: noq::Endpoint,
    },
    Tls {
        server: SocketAddr,
        connector: TlsConnector,
    },
}

impl Link {
    pub async fn connect(
        carrier: Carrier,
        server: SocketAddr,
        ca: &str,
        gso: bool,
    ) -> Result<Self, Error> {
        let tls = client_tls(ca)?;
        let kind = match carrier {
            Carrier::Quic | Carrier::QuicDatagram => {
                let endpoint =
                    noq::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;
                let crypto = QuicClientConfig::try_from(tls)?;
                let mut config = noq::ClientConfig::new(Arc::new(crypto));
                config.transport_config(transport(gso));
                let conn = endpoint.connect_with(config, server, SERVER_NAME)?.await?;
                Kind::Quic {
                    conn,
                    _endpoint: endpoint,
                }
            }
            Carrier::Tls => Kind::Tls {
                server,
                connector: TlsConnector::from(Arc::new(tls)),
            },
        };
        Ok(Self { carrier, gso, kind })
    }

    /// Opens a stream. A higher `priority` sends first on QUIC; TLS ignores it.
    pub async fn open(&self, priority: i32) -> Result<(Reader, Writer), Error> {
        match &self.kind {
            Kind::Quic { conn, .. } => {
                let (w, r) = conn.open_bi().await?;
                w.set_priority(priority)?;
                Ok((Box::new(r), Box::new(w)))
            }
            Kind::Tls { server, connector } => {
                let tcp = TcpStream::connect(server).await?;
                tcp.set_nodelay(true)?;
                let name = ServerName::try_from(SERVER_NAME)?;
                let tls = connector.connect(name, tcp).await?;
                let (r, w) = tokio::io::split(tls);
                Ok((Box::new(r), Box::new(w)))
            }
        }
    }

    /// Packets QUIC has lost so far; `None` on TLS.
    pub fn lost_packets(&self) -> Option<u64> {
        match &self.kind {
            Kind::Quic { conn, .. } => Some(conn.stats().lost_packets),
            Kind::Tls { .. } => None,
        }
    }

    /// The QUIC connection, when latency frames go in datagrams.
    pub fn datagrams(&self) -> Option<&noq::Connection> {
        match (&self.kind, self.carrier) {
            (Kind::Quic { conn, .. }, Carrier::QuicDatagram) => Some(conn),
            _ => None,
        }
    }
}

/// Accepts connections until the process stops.
pub async fn serve(
    carrier: Carrier,
    listen: SocketAddr,
    cert: &str,
    key: &str,
    gso: bool,
) -> Result<(), Error> {
    let tls = server_tls(cert, key)?;
    match carrier {
        Carrier::Quic | Carrier::QuicDatagram => {
            let crypto = QuicServerConfig::try_from(tls)?;
            let mut config = noq::ServerConfig::with_crypto(Arc::new(crypto));
            config.transport_config(transport(gso));
            let endpoint = noq::Endpoint::server(config, listen)?;
            while let Some(incoming) = endpoint.accept().await {
                tokio::spawn(report(quic_connection(incoming)));
            }
            Ok(())
        }
        Carrier::Tls => {
            let listener = TcpListener::bind(listen).await?;
            let acceptor = TlsAcceptor::from(Arc::new(tls));
            loop {
                let (tcp, _) = listener.accept().await?;
                tcp.set_nodelay(true)?;
                let acceptor = acceptor.clone();
                tokio::spawn(report(async move {
                    let (r, w) = tokio::io::split(acceptor.accept(tcp).await?);
                    test::handle(Box::new(r), Box::new(w)).await
                }));
            }
        }
    }
}

/// Echoes datagrams and serves each stream until the client closes.
async fn quic_connection(incoming: noq::Incoming) -> Result<(), Error> {
    let conn = incoming.await?;
    let echo = conn.clone();
    tokio::spawn(async move {
        while let Ok(datagram) = echo.read_datagram().await {
            if echo.send_datagram(datagram).is_err() {
                break;
            }
        }
    });
    while let Ok((w, r)) = conn.accept_bi().await {
        tokio::spawn(report(test::handle(Box::new(r), Box::new(w))));
    }
    Ok(())
}

async fn report(task: impl Future<Output = Result<(), Error>>) {
    if let Err(e) = task.await {
        eprintln!("server: {e}");
    }
}

fn transport(gso: bool) -> Arc<noq::TransportConfig> {
    let mut transport = noq::TransportConfig::default();
    transport.enable_segmentation_offload(gso);
    Arc::new(transport)
}

fn server_tls(cert: &str, key: &str) -> Result<rustls::ServerConfig, Error> {
    let certs = CertificateDer::pem_file_iter(cert)?.collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(key)?;
    Ok(
        rustls::ServerConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS13,
        ])
        .with_no_client_auth()
        .with_single_cert(certs, key)?,
    )
}

fn client_tls(ca: &str) -> Result<rustls::ClientConfig, Error> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca)? {
        roots.add(cert?)?;
    }
    Ok(
        rustls::ClientConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS13,
        ])
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}
