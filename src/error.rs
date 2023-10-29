use native_tls::Error as NativeTlsError;
use std::io::Error as IoError;
use thiserror::Error;
use url::ParseError;

/// The possible error types from the WebSocket connection.
#[derive(Error, Debug)]
pub enum WebSocketError {
    // connection errors
    /// Error connecting using TCP
    #[error("could not connect using TCP")]
    TcpConnectionError(IoError),
    /// Error connecting using TLS
    #[error("could not connect using TLS")]
    TlsConnectionError(NativeTlsError),
    /// Error building WebSocket with given TLS configuration
    #[error("could not build WebSocket with given TLS configuration")]
    TlsBuilderError(NativeTlsError),
    /// Error creating a TLS configuration (such as in method calls on
    /// [`TlsCertificate`](crate::secure::TlsCertificate) or
    /// [`TlsIdentity`](crate::secure::TlsIdentity))
    #[error("error with TLS configuration")]
    TlsConfigurationError(NativeTlsError),
    /// Attempted to use the WebSocket when it is already closed
    #[error("websocket is already closed")]
    WebSocketClosedError,
    /// Error shutting down the internal stream
    #[error("error shutting down stream")]
    ShutdownError(IoError),

    // handshake errors
    /// Invalid handshake response from the server
    #[error("invalid handshake response")]
    InvalidHandshakeError,
    /// The server rejected the handshake request
    #[error("server rejected handshake")]
    HandshakeFailedError {
        /// Status code from the server's handshake response
        status_code: String,
        /// Headers from the server's handshake response
        headers: Vec<(String, String)>,
        /// Body of the server's handshake response, if any
        body: Option<String>,
    },

    // frame errors
    /// Attempted to use a control frame whose payload is more than 125 bytes
    #[error("control frame has payload larger than 125 bytes")]
    ControlFrameTooLarge,
    /// Attempted to use a frame whose payload is too large
    #[error("payload is too large")]
    PayloadTooLarge,
    /// Received an invalid frame
    #[error("received frame is invalid")]
    InvalidFrame(InvalidFrameReason),
    /// Received a masked frame from the server
    #[error("received masked frame")]
    ReceivedMaskedFrame,

    // url errors
    /// URL could not be parsed
    #[error("url could not be parsed")]
    ParseError(ParseError),
    /// URL has invalid WebSocket scheme (use "ws" or "wss")
    #[error(r#"invalid websocket scheme (use "ws" or "wss")"#)]
    SchemeError,
    /// URL host is invalid or missing
    #[error("invalid or missing host")]
    HostError,
    /// URL port is invalid
    #[error("invalid or unknown port")]
    PortError,
    /// Could not parse URL into SocketAddrs
    #[error("could not parse into SocketAddrs")]
    SocketAddrError(IoError),
    /// Could not resolve the URL's domain
    #[error("could not resolve domain")]
    ResolutionError,

    // reading and writing
    /// Error reading from WebSocket
    #[error("could not read from WebSocket")]
    ReadError(IoError),
    /// Error writing to WebSocket
    #[error("could not write to WebSocket")]
    WriteError(IoError),

    // splitting
    /// Issue with mpsc channel
    #[error("error using channel")]
    ChannelError,
}

/// Delineation between types of malformed frames.
#[derive(Debug)]
pub enum InvalidFrameReason {
    /// The peer sent a continuation frame even though
    /// the previous message has finished with the `fin` flag
    FalseContinuation,

    /// The close code included inside of a `Close` frame.
    BadCloseCode,

    /// The peer sent a frame whose payload length specifier is 128 (127 max).
    BadPayloadLength,
    /// The payload length of a close frame was set to `1`.
    BadCloseFramePayload,
    /// The peer sent a text frame containing invalid UTF-8.
    BadUtf8,

    /// The peer sent a frame whose opcode is reserved for a data frame.
    ReservedDataOpcode,
    /// The peer sent a frame whose opcode is reserved for a control frame.
    ReservedControlOpcode,
}

/// A newtype to that implements [`From<std::io::Error>`]
/// and converts them to `WebSocketError::ReadError`
///
/// Useful for implementing the `Decoder` trait from `tokio_util`.
pub(crate) struct WsReadError(pub WebSocketError);

impl From<std::io::Error> for WsReadError {
    fn from(value: std::io::Error) -> Self {
        Self(WebSocketError::ReadError(value))
    }
}

/// A newtype to that implements [`From<std::io::Error>`]
/// and converts them to `WebSocketError::WriteError`
///
/// Useful for implementing the `Decoder` trait from `tokio_util`.
pub(crate) struct WsWriteError(pub WebSocketError);

impl From<std::io::Error> for WsWriteError {
    fn from(value: std::io::Error) -> Self {
        Self(WebSocketError::WriteError(value))
    }
}
