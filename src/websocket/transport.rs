use native_tls::TlsConnector as NativeTlsConnector;
use std::io::Error as IoError;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_native_tls::{TlsConnector as TokioTlsConnector, TlsStream};

use crate::error::WebSocketError;

/// An abstraction over a potentially TLS encoded TCP stream.
#[derive(Debug)]
pub(crate) enum Transport {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl Transport {
    /// Performs a TLS handshake if the transport is plain TCP and yields a TLS stream.
    pub async fn into_tls(
        self,
        host: &str,
        tls_connector: NativeTlsConnector,
    ) -> Result<Self, WebSocketError> {
        match self {
            Self::Plain(tcp_stream) => {
                let connector: TokioTlsConnector = tls_connector.into();
                let tls_stream = connector
                    .connect(host, tcp_stream)
                    .await
                    .map_err(|e| WebSocketError::TlsConnectionError(e))?;
                Ok(Transport::Tls(tls_stream))
            }
            Self::Tls(_) => Ok(self),
        }
    }
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Plain(tcp_stream) => Pin::new(tcp_stream).poll_read(cx, buf),
            Self::Tls(tls_stream) => Pin::new(tls_stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, IoError>> {
        match self.get_mut() {
            Self::Plain(tcp_stream) => Pin::new(tcp_stream).poll_write(cx, buf),
            Self::Tls(tls_stream) => Pin::new(tls_stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        match self.get_mut() {
            Self::Plain(tcp_stream) => Pin::new(tcp_stream).poll_flush(cx),
            Self::Tls(tls_stream) => Pin::new(tls_stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        match self.get_mut() {
            Self::Plain(tcp_stream) => Pin::new(tcp_stream).poll_shutdown(cx),
            Self::Tls(tls_stream) => Pin::new(tls_stream).poll_shutdown(cx),
        }
    }
}
