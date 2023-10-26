pub mod builder;
pub mod frame;
mod handshake;
pub mod message;
pub mod ops;
mod parsed_addr;
mod socket;
pub mod split;

use std::task::{ready, Poll};

use crate::error::WebSocketError;
use builder::WebSocketBuilder;
use futures::{Sink, SinkExt, Stream, StreamExt};
use split::{WebSocketReadHalf, WebSocketWriteHalf};

use self::{message::Message, ops::Pong};

/// Manages the WebSocket connection; used to connect, send data, and receive data.
///
/// Connect with [`WebSocket::connect()`]:
///
/// ```
/// # use websockets::{WebSocket, WebSocketError};
/// # #[tokio::main]
/// # async fn main() -> Result<(), WebSocketError> {
/// let mut ws = WebSocket::connect("wss://echo.websocket.org/").await?;
/// # Ok(())
/// # }
/// ```
///
/// Customize the handshake using a [`WebSocketBuilder`] obtained from [`WebSocket::builder()`]:
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
///
/// Use the `WebSocket::send*` methods to send frames:
///
/// ```
/// # use websockets::{WebSocket, WebSocketError};
/// # #[tokio::main]
/// # async fn main() -> Result<(), WebSocketError> {
/// # let mut ws = WebSocket::connect("wss://echo.websocket.org")
/// #     .await?;
/// ws.send_text("foo".to_string()).await?;
/// # Ok(())
/// # }
/// ```
///
/// Use [`WebSocket::receive()`] to receive frames:
///
/// ```
/// # use websockets::{WebSocket, WebSocketError, Frame};
/// # #[tokio::main]
/// # async fn main() -> Result<(), WebSocketError> {
/// # let mut ws = WebSocket::connect("wss://echo.websocket.org")
/// #     .await?;
/// # ws.send_text("foo".to_string()).await?;
/// if let Frame::Text { payload: received_msg, .. } =  ws.receive().await? {
///     // echo.websocket.org echoes text frames
///     assert_eq!(received_msg, "foo".to_string());
/// }
/// # else { panic!() }
/// # Ok(())
/// # }
/// ```
///
/// Close the connection with [`WebSocket::close()`]:
///
/// ```
/// # use websockets::{WebSocket, WebSocketError, Frame};
/// # #[tokio::main]
/// # async fn main() -> Result<(), WebSocketError> {
/// #     let mut ws = WebSocket::connect("wss://echo.websocket.org")
/// #         .await?;
/// ws.close(Some((1000, String::new()))).await?;
/// if let Frame::Close{ payload: Some((status_code, _reason)) } = ws.receive().await? {
///     assert_eq!(status_code, 1000);
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Splitting
///
/// To facilitate simulataneous reads and writes, the `WebSocket` can be split
/// into a [read half](WebSocketReadHalf) and a [write half](WebSocketWriteHalf).
/// The read half allows frames to be received, while the write half
/// allows frames to be sent.
///
/// If the read half receives a Ping or Close frame, it needs to send a
/// Pong or echo the Close frame and close the WebSocket, respectively.
/// The write half is notified of these events, but it cannot act on them
/// unless it is flushed. Events can be explicitly [`flush`](WebSocketWriteHalf::flush_events())ed,
/// but sending a frame will also flush events. If frames are not being
/// sent frequently, consider explicitly flushing events.
///
/// Flushing is done automatically if you are using the the `WebSocket` type by itself.
#[derive(Debug)]
pub struct WebSocket {
    inner: FlushingWs,

    accepted_subprotocol: Option<String>,
    handshake_response_headers: Option<Vec<(String, String)>>,
}

impl WebSocket {
    /// Constructs a [`WebSocketBuilder`], which can be used to customize
    /// the WebSocket handshake.
    pub fn builder() -> WebSocketBuilder {
        WebSocketBuilder::new()
    }

    /// Connects to a URL (and performs the WebSocket handshake).
    pub async fn connect(url: &str) -> Result<Self, WebSocketError> {
        WebSocketBuilder::new().connect(url).await
    }

    /// Receives a [`Frame`] over the WebSocket connection.
    ///
    /// If the received frame is a Ping frame, a Pong frame will be sent.
    /// If the received frame is a Close frame, an echoed Close frame
    /// will be sent and the WebSocket will close.
    pub async fn receive(&mut self) -> Result<Message, WebSocketError> {
        match <Self as StreamExt>::next(self).await {
            Some(Ok(v)) => Ok(v),
            Some(Err(err)) => Err(err),
            None => Err(WebSocketError::WebSocketClosedError),
        }
    }

    /// Sends a Text frame over the WebSocket connection, constructed
    /// from passed arguments. `continuation` will be `false` and `fin` will be `true`.
    /// To use a custom `continuation` or `fin`, construct a [`Frame`] and use
    /// [`WebSocket::send()`].
    pub async fn send_text(&mut self, payload: String) -> Result<(), WebSocketError> {
        self.send(Message::Text(payload)).await
    }

    /// Sends a Binary frame over the WebSocket connection, constructed
    /// from passed arguments. `continuation` will be `false` and `fin` will be `true`.
    /// To use a custom `continuation` or `fin`, construct a [`Frame`] and use
    /// [`WebSocket::send()`].
    pub async fn send_binary(&mut self, payload: Vec<u8>) -> Result<(), WebSocketError> {
        self.send(Message::Binary(payload)).await
    }

    /// Sends a Close frame over the WebSocket connection, constructed
    /// from passed arguments, and closes the WebSocket connection.
    /// This method will attempt to wait for an echoed Close frame,
    /// which is returned.
    pub async fn close(mut self, payload: Option<(u16, String)>) -> Result<(), WebSocketError> {
        self.inner.write.close(payload).await
    }

    /// Sends a Ping frame over the WebSocket connection, constructed
    /// from passed arguments.
    pub fn send_ping(&mut self, payload: Option<Vec<u8>>) -> Pong {
        todo!()
    }

    /// Shuts down the WebSocket connection **without sending a Close frame**.
    /// It is recommended to use the [`close()`](WebSocket::close()) method instead.
    pub async fn drop(mut self) -> Result<(), WebSocketError> {
        self.inner.write.drop().await
    }

    /// Splits the WebSocket into a read half and a write half, which can be used separately.
    /// [Accepted subprotocol](WebSocket::accepted_subprotocol())
    /// and [handshake response headers](WebSocket::handshake_response_headers()) data
    /// will be lost.
    pub fn split(self) -> (WebSocketReadHalf, WebSocketWriteHalf) {
        (self.inner.read, self.inner.write)
    }

    /// Joins together a split read half and write half to reconstruct a WebSocket.
    pub fn join(read_half: WebSocketReadHalf, write_half: WebSocketWriteHalf) -> Self {
        Self {
            inner: FlushingWs {
                read: read_half,
                write: write_half,
                received_message: None,
            },
            accepted_subprotocol: None,
            handshake_response_headers: None,
        }
    }

    /// Returns the subprotocol that was accepted by the server during the handshake,
    /// if any. This data will be lost if the WebSocket is [`split`](WebSocket::split()).
    pub fn accepted_subprotocol(&self) -> &Option<String> {
        // https://tools.ietf.org/html/rfc6455#section-1.9
        &self.accepted_subprotocol
    }

    /// Returns the headers that were returned by the server during the handshake.
    /// This data will be lost if the WebSocket is [`split`](WebSocket::split()).
    pub fn handshake_response_headers(&self) -> &Option<Vec<(String, String)>> {
        // https://tools.ietf.org/html/rfc6455#section-4.2.2
        &self.handshake_response_headers
    }
}

impl Stream for WebSocket {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl Sink<Message> for WebSocket {
    type Error = <WebSocketWriteHalf as Sink<Message>>::Error;

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.inner.write.start_send_unpin(item)
    }

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.write.poll_ready_unpin(cx)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.write.poll_flush_unpin(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.write.poll_close_unpin(cx)
    }
}

#[derive(Debug)]
struct FlushingWs {
    pub read: WebSocketReadHalf,
    pub write: WebSocketWriteHalf,

    pub received_message: Option<Message>,
}

impl Stream for FlushingWs {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.received_message.is_none() {
            self.received_message = match ready!(self.read.poll_next_unpin(cx)) {
                Some(Ok(res)) => Some(res),
                Some(Err(err)) => return Poll::Ready(Some(Err(err))),
                None => return Poll::Ready(None),
            }
        };

        ready!(self.write.poll_flush_unpin(cx))?;

        Poll::Ready(Some(Ok(self
            .received_message
            .take()
            .unwrap_or_else(|| unreachable!()))))
    }
}
