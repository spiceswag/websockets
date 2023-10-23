use std::pin::Pin;
use std::task::Poll;

use flume::{Receiver, Sender};
use futures::{ready, Sink, SinkExt, Stream, StreamExt};
use tokio::io::{BufReader, ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite};

use super::frame::{Frame, WsFrameCodec};
use super::message::{Fragmentation, Message, MessageFragment};
use super::stream::Stream as Socket;
#[allow(unused_imports)] // for intra doc links
use super::WebSocket;
use crate::batched::{Batched, Prioritized};
use crate::error::{WebSocketError, WsWriteError};

/// Events sent from the read half to the write half
#[derive(Debug)]
pub(super) enum Event {
    SendPongFrame(Frame),
    SendCloseFrameAndShutdown(Frame),
}

/// The read half of a WebSocket connection, generated from [`WebSocket::split()`].
/// This half can only receive frames.
#[derive(Debug)]
pub struct WebSocketReadHalf {
    pub(super) stream: FramedRead<BufReader<ReadHalf<Socket>>, WsFrameCodec>,
    pub(super) sender: Sender<Event>,

    // a message that has not fully been received yet
    pub(super) partial_message: Option<Message>,
}

impl WebSocketReadHalf {
    /// Receives a [`Message`] over the WebSocket connection.
    ///
    /// If the received frame is a Ping frame, an event to send a Pong frame will be queued.
    /// If the received frame is a Close frame, an event to send a Close frame
    /// will be queued and the WebSocket will close. However, events are not
    /// acted upon unless flushed (see the documentation on the [`WebSocket`](WebSocket#splitting)
    /// type for more details).
    pub async fn receive(&mut self) -> Result<Message, WebSocketError> {
        match self.next().await {
            None => Err(WebSocketError::WebSocketClosedError),
            Some(res) => res,
        }
    }
}

impl Stream for WebSocketReadHalf {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // piece together a frame

        let frame = match ready!(self.stream.poll_next_unpin(cx)) {
            Some(Ok(f)) => f,
            Some(Err(err)) => return Poll::Ready(Some(Err(err.0))),
            None => return Poll::Ready(None),
        };

        match frame {
            Frame::Ping { payload } => {
                self.sender
                    .send(Event::SendPongFrame(Frame::Pong { payload }))
                    .map_err(|_| WebSocketError::ChannelError)?;
                Poll::Pending
            }
            Frame::Close { payload } => {
                self.sender
                    .send(Event::SendCloseFrameAndShutdown(Frame::Close { payload }))
                    .map_err(|_| WebSocketError::ChannelError)?;
                Poll::Ready(None)
            }
            Frame::Binary {
                payload,
                continuation,
                fin,
            } => {
                let complete = match self.partial_message.take() {
                    Some(Message::Binary(mut prev_payloads)) if continuation => {
                        prev_payloads.extend(payload);
                        prev_payloads
                    }
                    None if !continuation => payload,
                    _ => unreachable!(),
                };

                if fin {
                    Poll::Ready(Some(Ok(Message::Binary(complete))))
                } else {
                    self.partial_message = Some(Message::Binary(complete));
                    Poll::Pending
                }
            }
            Frame::Text {
                payload,
                continuation,
                fin,
            } => {
                let complete = match self.partial_message.take() {
                    Some(Message::Text(mut prev_payloads)) if continuation => {
                        prev_payloads.extend(payload.chars());
                        prev_payloads
                    }
                    None if !continuation => payload,
                    _ => unreachable!(),
                };

                if fin {
                    Poll::Ready(Some(Ok(Message::Text(complete))))
                } else {
                    self.partial_message = Some(Message::Text(complete));
                    Poll::Pending
                }
            }

            _ => Poll::Pending,
        }
    }
}

impl WebSocketReadHalf {
    /// Receive the next message fragmented as it comes in.
    ///
    /// Useful for large messages where it is more optimal to process it as it
    /// comes in, rather than all at once.
    pub fn receive_fragmented<'a>(&'a mut self) -> FragmentedMessage<'a> {
        todo!()
    }
}

/// A [`Stream`] yielding the next message fragmented,
/// as it is received from the remote peer.
///
/// Useful for large messages where it is more optimal to process it as it
/// comes in, rather than all at once.
///
/// # Receiving
///
/// This type implements [Stream<Item = Result<Message, WebSocketError>], but returned "`Messages`"
/// are not actual messages but parts of a single fragmented message.
///
/// Be mindful of the fact that the current implementation does not
/// account for the in-spec edge case where a UTF-8 sequence is split over 2 frames.
#[derive(Debug)]
pub struct FragmentedMessage<'a> {
    read: &'a mut WebSocketReadHalf,
}

impl<'a> Stream for FragmentedMessage<'a> {
    type Item = Result<MessageFragment, WebSocketError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        todo!()
    }
}

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
    pub(super) stream: Batched<FramedWrite<WriteHalf<Socket>, WsFrameCodec>, Frame>,
    pub(super) fragmentation: Fragmentation,

    /// Event receiver
    pub(super) receiver: Receiver<Event>,
    /// A flag that signals that the sink is trying to shut down or already has shut down
    pub(super) shutdown: bool,
    /// A flag that is raised when we are the first party to send a close frame
    pub(super) sent_closed: bool,
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

    /// Shuts down the WebSocket connection **without sending a Close frame**.
    /// It is recommended to use the [`close()`](WebSocketWriteHalf::close()) method instead.
    pub async fn drop(&mut self) -> Result<(), WebSocketError> {
        SinkExt::close(self).await?;

        self.shutdown = true;
        // Stops us from sending a second close frame after receiving the close frame that the peer echoed
        self.sent_closed = true;
        Ok(())
    }

    /// Sends a Close frame over the WebSocket connection, constructed
    /// from passed arguments, and closes the WebSocket connection.
    ///
    /// As per the WebSocket protocol, the server should send a Close frame in response
    /// upon receiving a Close frame. Although the write half will be closed,
    /// the server's echoed Close frame can be read from the still open read half.
    ///
    /// This method will flush incoming events.
    /// See the documentation on the [`WebSocket`](WebSocket#splitting) type for more details
    /// about events.
    pub async fn close(&mut self, payload: Option<(u16, String)>) -> Result<(), WebSocketError> {
        // https://tools.ietf.org/html/rfc6455#section-5.5.1
        self.stream
            .send(Frame::Close { payload })
            .await
            .map_err(|err| err.0)?;
        // self.shutdown().await?;
        Ok(())
    }

    /// Sends a Ping frame over the WebSocket connection, constructed
    /// from passed arguments.
    ///
    /// This method will flush incoming events.
    /// See the documentation on the [`WebSocket`](WebSocket#splitting) type for more details
    /// about events.
    pub async fn send_ping(&mut self, payload: Option<Vec<u8>>) -> Result<(), WebSocketError> {
        // https://tools.ietf.org/html/rfc6455#section-5.5.2
        self.stream
            .send(Frame::Ping { payload })
            .await
            .map_err(|err| err.0)
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
            return self.poll_close_unpin(cx);
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
                    if self.sent_closed {
                        //     events get priority \/\/\/
                        self.stream
                            .start_send_unpin(Prioritized(frame))
                            .map_err(|err| err.0)?;

                        // Not needed because we're already trying to close the sink when the shutdown flag is up
                        // self.drop().await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_send_sync()
    where
        WebSocketReadHalf: Send + Sync,
        WebSocketWriteHalf: Send + Sync,
    {
    }
}
