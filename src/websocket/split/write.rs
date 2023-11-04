use std::{pin::Pin, task::Poll};

use flume::{Receiver, Sender};
use futures::{Sink, SinkExt};
use tokio::{io::WriteHalf, sync::oneshot};
use tokio_util::codec::FramedWrite;

use crate::{
    batched::*,
    error::WsWriteError,
    ops::{ClosePayload, PingPayload, Pong},
    websocket::{
        frame::{Frame, WsFrameCodec},
        message::Fragmentation,
        socket::Socket,
    },
    Message, WebSocketError,
};

use super::Event;

/// The write half of a WebSocket connection, generated from [`WebSocket::split()`].
/// This half can only send frames.
///
/// # Sending messages
///
/// Sending messages through this structure is as simple as using the [`Sink`] implementation.
/// The fragmentation strategy in use can be adjusted
///
#[derive(Debug)]
pub struct WebSocketWriteHalf {
    pub(crate) stream: Batched<FramedWrite<WriteHalf<Socket>, WsFrameCodec>, Frame>,
    pub(crate) fragmentation: Fragmentation,

    /// Event receiver
    pub(crate) receiver: Receiver<Event>,
    /// A flag that signals that the sink is trying to shut down or already has shut down
    pub(crate) shutdown: bool,
    /// A flag that is raised when we are the first party to send a close frame
    pub(crate) sent_closed: bool,

    /// Send pong wake up handles
    pub(crate) pong_sender: Sender<oneshot::Sender<PingPayload>>,
}

impl WebSocketWriteHalf {
    /// Flushes incoming events from the read half. If the read half received a Ping frame,
    /// a Pong frame will be sent. If the read half received a Close frame,
    /// an echoed Close frame will be sent and the WebSocket will close.
    ///
    /// See the documentation on the [`WebSocket`](WebSocket#splitting) type for more details
    /// about events.
    pub async fn flush_events(&mut self) -> Result<(), WebSocketError> {
        while let Ok(event) = self.receiver.try_recv() {
            if self.shutdown {
                break;
            }
            match event {
                Event::SendPongFrame(frame) => {
                    self.stream.send(frame).await.map_err(|err| err.0)?
                }
                Event::SendCloseFrameAndShutdown(frame) => {
                    // read half will always send this event if it has received a close frame,
                    // but if we have sent one already, then we have sent and received a close
                    // frame, so we will shutdown
                    if self.sent_closed {
                        self.stream.send(frame).await.map_err(|err| err.0)?;
                        self.drop().await?;
                    }
                }
            };
        }
        Ok(())
    }

    /// Sends a Ping frame over the WebSocket connection, constructed
    /// from passed arguments.
    ///
    /// This `async` method returns a result containing another future.
    /// By polling the first future you send the ping packet to the peer.
    /// By polling the nested future you wait for the peer to respond with a pong frame.
    ///
    /// The pong future will not resolve if no frames are being read from the read half of the web socket.
    ///
    /// This method will flush incoming events.
    /// See the documentation on the [`WebSocket`](WebSocket#splitting) type for more details
    /// about events.
    pub async fn send_ping(&mut self, payload: Option<Vec<u8>>) -> Result<Pong, WebSocketError> {
        // https://tools.ietf.org/html/rfc6455#section-5.5.2

        let (send, recv) = oneshot::channel();

        self.stream
            .send(Frame::Ping { payload })
            .await
            .map_err(|err| err.0)?;

        let _ = self.pong_sender.send(send);

        Ok(Pong { recv })
    }

    /// Shuts down the send half of the TCP connection **without** sending a close frame.
    pub async fn drop(&mut self) -> Result<(), WebSocketError> {
        SinkExt::close(self).await?;

        self.shutdown = true;
        // Stops us from sending a second close frame after receiving the close frame that the peer echoed
        self.sent_closed = true;
        Ok(())
    }

    /// Performs a clean shutdown on the WebSocket connection by sending
    /// a `Close` frame with the desired payload.
    ///
    /// This method completes when the server echoes the `Close` frame,
    /// and closes the TCP connection, or if the server is too slow, we close it first.
    ///
    /// This method will flush incoming events.
    /// See the documentation on the [`WebSocket`](WebSocket#splitting) type for more details
    /// about events.
    pub async fn close(mut self, payload: Option<ClosePayload>) -> Result<(), WebSocketError> {
        let payload = if let Some(payload) = payload {
            if !payload.status.sendable() {
                return Err(WebSocketError::BadStatus);
            }

            let reason = payload.reason.unwrap_or_else(String::new);
            let status = payload.status.into();

            Some((status, reason))
        } else {
            None
        };

        // https://tools.ietf.org/html/rfc6455#section-5.5.1
        self.stream
            .send(Frame::Close { payload })
            .await
            .map_err(|err| err.0)?;

        // ensure no stuff can be sent
        self.drop().await?;

        Ok(())
    }
}

impl Sink<Message> for WebSocketWriteHalf {
    type Error = WebSocketError;

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        if self.shutdown || self.sent_closed {
            return Err(WebSocketError::WebSocketClosedError);
        }

        let frames = self.fragmentation.fragment(item);

        self.stream.start_send_unpin(frames).map_err(|err| err.0)
    }

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        if self.shutdown {
            // no need to map errors to shutdown error
            return match self.poll_close_unpin(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(_)) => Poll::Ready(Err(WebSocketError::WebSocketClosedError)),
                err @ _ => err,
            };
        }

        // try to feed events into the sink
        while let Ok(event) = self.receiver.try_recv() {
            // make sure we're ready for feeding
            match <Batched::<FramedWrite<WriteHalf<Socket>, WsFrameCodec>, Frame> as SinkExt<Frame>>::poll_ready_unpin(
                &mut self.stream,
                cx,
            )
                .map_err(|err: WsWriteError| err.0)
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                _ => {}
            }

            match event {
                Event::SendPongFrame(frame) => {
                    //  events get priority over other frames \/\/\/
                    self.stream
                        .start_send_unpin(Prioritized(frame))
                        .map_err(|err| err.0)?
                }
                Event::SendCloseFrameAndShutdown(frame) => {
                    // read half will always send this event if it has received a close frame,
                    // but if we have sent one already, then we have sent and received a close
                    // frame, so we will shutdown
                    if !self.sent_closed {
                        //     events get priority \/\/\/
                        self.stream
                            .start_send_unpin(Prioritized(frame))
                            .map_err(|err| err.0)?;

                        self.shutdown = true;
                    }
                }
            };
        }

        // The fact that we poll twice before a send
        // introduces a sort of dos attack where the server spams
        // pings and we can't send any actual data in return.

        <Batched::<FramedWrite<WriteHalf<Socket>, WsFrameCodec>, Frame> as SinkExt<Frame>>::poll_ready_unpin(
            &mut self.stream,
            cx,
        )
        .map_err(|err| err.0)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        <Batched::<FramedWrite<WriteHalf<Socket>, WsFrameCodec>, Frame> as SinkExt<Frame>>::poll_flush_unpin(
            &mut self.stream,
            cx,
        )
            .map_err(|err| err.0)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        // this is not that undesired considering you can just call WebSocketWriteHalf::drop
        self.shutdown = true;

        <Batched::<FramedWrite<WriteHalf<Socket>, WsFrameCodec>, Frame> as SinkExt<Frame>>::poll_close_unpin(
            &mut self.stream,
            cx,
        )
            .map_err(|err| err.0)
            .map_err(|err| match err {
                // From<std::io::Error> (used by Framed) yields this type of error so we have to transform it to shutdown error
                WebSocketError::WriteError(err) => WebSocketError::ShutdownError(err),
                _ => err,
            })
    }
}
