use std::convert::TryFrom;
use std::fmt::{Debug, Error as FmtError, Formatter};

use native_tls::{
    TlsConnector as NativeTlsConnector, TlsConnectorBuilder as NativeTlsConnectorBuilder,
};
use tokio::io::{self, BufReader};
use tokio::net::TcpStream;
use tokio_util::codec::{FramedRead, FramedWrite};

use super::frame::WsFrameCodec;
use super::handshake::Handshake;
use super::message::Fragmentation;
use super::parsed_addr::ParsedAddr;
use super::socket::Socket;
use super::split::{WebSocketReadHalf, WebSocketWriteHalf};
use super::{FlushingWs, WebSocket};
use crate::batched::Batched;
use crate::error::WebSocketError;
use crate::secure::{TlsCertificate, TlsIdentity, TlsProtocol};

/// A builder used to customize the WebSocket handshake.
///
/// Handshake headers as well as subprotocols can be added and removed.
/// Methods prefixed with `tls_` allow for the customization of a secure
/// WebSocket connection.
///
/// ```
/// # use websockets::{WebSocket, WebSocketError};
/// # #[tokio::main]
/// # async fn main() -> Result<(), WebSocketError> {
/// let mut ws = WebSocket::builder()
///     .add_subprotocol("wamp")
///     .connect("wss://echo.websocket.org")
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct WebSocketBuilder {
    fragmentation: Fragmentation,
    additional_handshake_headers: Vec<(String, String)>,
    subprotocols: Vec<String>,
    tls_connector_builder: NativeTlsConnectorBuilder,
}

impl Debug for WebSocketBuilder {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        f.write_str("WebSocketBuilder")
    }
}

impl WebSocketBuilder {
    pub(super) fn new() -> Self {
        Self {
            fragmentation: Fragmentation::None,
            additional_handshake_headers: Vec::new(),
            subprotocols: Vec::new(),
            tls_connector_builder: NativeTlsConnector::builder(),
        }
    }

    /// Builds a [`WebSocket`] using this builder, then connects to a URL
    /// (and performs the WebSocket handshake).
    pub async fn connect(self, url: &str) -> Result<WebSocket, WebSocketError> {
        let parsed_addr = ParsedAddr::try_from(url)?;

        let stream = Socket::Plain(
            TcpStream::connect(parsed_addr.addr)
                .await
                .map_err(|e| WebSocketError::TcpConnectionError(e))?,
        );
        let stream = match &parsed_addr.scheme[..] {
            // https://tools.ietf.org/html/rfc6455#section-11.1.1
            "ws" => stream,
            // https://tools.ietf.org/html/rfc6455#section-11.1.2
            "wss" => {
                let tls_config = self
                    .tls_connector_builder
                    .build()
                    .map_err(|e| WebSocketError::TlsBuilderError(e))?;
                stream.into_tls(&parsed_addr.host, tls_config).await?
            }
            _ => return Err(WebSocketError::SchemeError),
        };

        let (event_sender, event_receiver) = flume::unbounded();
        let (pong_handle_sender, pong_handle_receiver) = flume::unbounded();

        let (read_half, write_half) = io::split(stream);
        let mut ws = WebSocket {
            inner: FlushingWs {
                read: WebSocketReadHalf {
                    stream: FramedRead::new(BufReader::new(read_half), WsFrameCodec::new()),
                    sender: event_sender,
                    partial_message: None,
                    pong_receiver: pong_handle_receiver,
                    pongs: vec![],
                },
                write: WebSocketWriteHalf {
                    stream: Batched::new(FramedWrite::new(write_half, WsFrameCodec::new()), 16),
                    fragmentation: self.fragmentation,
                    receiver: event_receiver,
                    shutdown: false,
                    sent_closed: false,
                    pong_sender: pong_handle_sender,
                },
                received_message: None,
            },
            accepted_subprotocol: None,
            handshake_response_headers: None,
        };

        // perform opening handshake
        let handshake = Handshake::new(
            &parsed_addr,
            &self.additional_handshake_headers,
            &self.subprotocols,
        );
        handshake.send_request(&mut ws).await?;
        match handshake.check_response(&mut ws).await {
            Ok(_) => Ok(ws.into()),
            Err(e) => {
                ws.drop().await?;
                Err(e)
            }
        }
    }

    /// Define the fragmentation strategy employed by this websocket.
    /// Defaults to doing no fragmentation and praying no frames exceed the peer's limit.
    pub fn fragmentation(mut self, strategy: Fragmentation) -> Self {
        self.fragmentation = strategy;
        self
    }

    /// Adds a header to be sent in the WebSocket handshake.
    pub fn add_header(mut self, header_name: &str, header_value: &str) -> Self {
        // https://tools.ietf.org/html/rfc6455#section-4.2.2
        self.additional_handshake_headers
            .push((header_name.to_string(), header_value.to_string()));
        self
    }

    /// Removes a header which would be sent in the WebSocket handshake.
    pub fn remove_header(mut self, header_name: &str) -> Self {
        // https://tools.ietf.org/html/rfc6455#section-4.2.2
        self.additional_handshake_headers
            .retain(|header| header.0 != header_name);
        self
    }

    /// Adds a subprotocol to the list of subprotocols to be sent in the
    /// WebSocket handshake. The server may select a subprotocol from this list.
    /// If it does, the selected subprotocol can be found using the
    /// [`WebSocket::accepted_subprotocol()`] method.
    pub fn add_subprotocol(mut self, subprotocol: &str) -> Self {
        // https://tools.ietf.org/html/rfc6455#section-1.9
        self.subprotocols.push(subprotocol.to_string());
        self
    }

    /// Removes a subprotocol from the list of subprotocols that would be sent
    /// in the WebSocket handshake.
    pub fn remove_subprotocol(mut self, subprotocol: &str) -> Self {
        // https://tools.ietf.org/html/rfc6455#section-1.9
        self.subprotocols.retain(|s| s != subprotocol);
        self
    }

    /// Controls the use of certificate validation. Defaults to false.
    pub fn tls_danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> Self {
        self.tls_connector_builder
            .danger_accept_invalid_certs(accept_invalid_certs);
        self
    }

    /// Controls the use of hostname verification. Defaults to false.
    pub fn tls_danger_accept_invalid_hostnames(mut self, accept_invalid_hostnames: bool) -> Self {
        self.tls_connector_builder
            .danger_accept_invalid_hostnames(accept_invalid_hostnames);
        self
    }

    /// Adds a certificate to the set of roots that the connector will trust.
    /// The connector will use the system's trust root by default. This method can be used to add
    /// to that set when communicating with servers not trusted by the system.
    /// Defaults to an empty set.
    pub fn tls_add_root_certificate(mut self, cert: TlsCertificate) -> Self {
        self.tls_connector_builder.add_root_certificate(cert.0);
        self
    }

    /// Controls the use of built-in system certificates during certificate validation.
    /// Defaults to false -- built-in system certs will be used.
    pub fn tls_disable_built_in_roots(mut self, disable: bool) -> Self {
        self.tls_connector_builder.disable_built_in_roots(disable);
        self
    }

    /// Sets the identity to be used for client certificate authentication.
    pub fn tls_identity(mut self, identity: TlsIdentity) -> Self {
        self.tls_connector_builder.identity(identity.0);
        self
    }

    /// Sets the maximum supported TLS protocol version.
    /// A value of None enables support for the newest protocols supported by the implementation.
    /// Defaults to None.
    pub fn tls_max_protocol_version(mut self, protocol: Option<TlsProtocol>) -> Self {
        self.tls_connector_builder.max_protocol_version(protocol);
        self
    }

    /// Sets the minimum supported TLS protocol version.
    /// A value of None enables support for the oldest protocols supported by the implementation.
    /// Defaults to Some(Protocol::Tlsv10).
    pub fn tls_min_protocol_version(mut self, protocol: Option<TlsProtocol>) -> Self {
        self.tls_connector_builder.min_protocol_version(protocol);
        self
    }

    /// Controls the use of Server Name Indication (SNI).
    /// Defaults to true.
    pub fn tls_use_sni(mut self, use_sni: bool) -> Self {
        self.tls_connector_builder.use_sni(use_sni);
        self
    }
}
