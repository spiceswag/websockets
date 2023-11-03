use std::pin::Pin;
use std::task::Poll;

use flume::{Receiver, Sender};
use futures::stream::FusedStream;
use futures::{ready, Sink, SinkExt, Stream, StreamExt};
use tokio::io::{BufReader, ReadHalf, WriteHalf};
use tokio::sync::oneshot;
use tokio_util::codec::{FramedRead, FramedWrite};

use super::frame::FrameType;
use super::frame::{Frame, WsFrameCodec};
use super::message::{Fragmentation, IncompleteMessage, Message, MessageFragment};
use super::socket::Socket;
#[allow(unused_imports)] // for intra doc links
use super::WebSocket;
use crate::batched::{Batched, Prioritized};
use crate::error::{InvalidFrame, WebSocketError, WsWriteError};
use crate::ops::{ClosePayload, PingPayload, Pong};

/// The read half of a WebSocket connection, generated from [`WebSocket::split()`].
/// This half can only receive frames.
#[derive(Debug)]
pub struct WebSocketReadHalf {
    pub(super) stream: FramedRead<BufReader<ReadHalf<Socket>>, WsFrameCodec>,
    pub(super) sender: Sender<Event>,

    /// Part of a message that has not fully been received yet.
    pub(super) partial_message: Option<IncompleteMessage>,

    /// A receiver for pong future handles.
    pub(super) pong_receiver: Receiver<oneshot::Sender<PingPayload>>,
    /// A list of handles for waking up pong futures.
    pub(super) pongs: Vec<oneshot::Sender<PingPayload>>,
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

    /// Collect pongs sent by the send half of the socket.
    fn collect_pongs(&mut self) {
        while let Ok(handle) = self.pong_receiver.try_recv() {
            self.pongs.push(handle);
        }
    }

    /// Wake up pong futures by their handles
    fn wake_up_pongs(&mut self, payload: Option<Vec<u8>>) {
        self.pongs.drain(..).for_each(|sender| {
            let _ = sender.send(PingPayload {
                payload: payload.as_ref().filter(|vec| !vec.is_empty()).cloned(),
            });
        });
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

        self.collect_pongs();

        match frame {
            Frame::Ping { payload } => {
                self.sender
                    .send(Event::SendPongFrame(Frame::Pong { payload }))
                    .map_err(|_| WebSocketError::ChannelError)?;

                Poll::Pending
            }
            Frame::Pong { payload } => {
                self.wake_up_pongs(payload);
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
                    Some(IncompleteMessage::Binary(mut prev_payloads)) if continuation => {
                        prev_payloads.extend(payload.iter());
                        prev_payloads
                    }
                    // into_owned() is ok because received frames are 'static
                    None if !continuation => payload,
                    _ => unreachable!(),
                };

                if fin {
                    Poll::Ready(Some(Ok(Message::Binary(complete))))
                } else {
                    self.partial_message = Some(IncompleteMessage::Binary(complete));
                    Poll::Pending
                }
            }
            Frame::Text {
                payload,
                continuation,
                fin,
            } => {
                let complete = match self.partial_message.take() {
                    Some(IncompleteMessage::Text(mut prev_payloads)) if continuation => {
                        prev_payloads.extend(payload);
                        prev_payloads
                    }
                    None if !continuation => payload,
                    _ => unreachable!(),
                };

                if fin {
                    Poll::Ready(Some(Ok(Message::Text(
                        String::from_utf8(complete).map_err(|_| {
                            WebSocketError::InvalidFrame(crate::error::InvalidFrame::BadUtf8)
                        })?,
                    ))))
                } else {
                    self.partial_message = Some(IncompleteMessage::Text(complete));
                    Poll::Pending
                }
            }
        }
    }
}

impl WebSocketReadHalf {
    /// Receive the next message fragmented as it comes in.
    ///
    /// Useful for large messages where it is more optimal to process it as it
    /// comes in, rather than all at once.
    pub fn receive_fragmented<'a>(&'a mut self) -> FragmentedMessage<'a> {
        FragmentedMessage {
            read: self,
            data_type: None,
            staggered_utf8: None,
            first_arrived: false,
            fin: false,
        }
    }
}

/// A [`Stream`] yielding the next message fragmented,
/// as it is received from the remote peer.
///
/// Useful for large messages where it is more optimal to process it as it
/// comes in, rather than all at once.
#[derive(Debug)]
pub struct FragmentedMessage<'a> {
    /// The handle to the web socket read half
    read: &'a mut WebSocketReadHalf,

    /// The expected frame type.
    data_type: Option<FrameType>,

    /// Any UTF-8 bytes staggered as outlined in the documentation of [`MessageFragment::Text`]
    staggered_utf8: Option<Vec<u8>>,

    /// If the first frame of the message has been received.
    /// Useful for checking for invalid frames.
    first_arrived: bool,
    /// If the message is finished.
    fin: bool,
}

impl<'a> Stream for FragmentedMessage<'a> {
    type Item = Result<MessageFragment, WebSocketError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.fin {
            return Poll::Ready(None);
        }

        let frame = match ready!(self.read.stream.poll_next_unpin(cx)) {
            Some(Ok(f)) => f,
            Some(Err(err)) => return Poll::Ready(Some(Err(err.0))),
            None => return Poll::Ready(None),
        };

        if self.data_type.is_none() {
            self.data_type = Some(frame.frame_type())
        }

        // No need to check for frame type equivalence because its derived in the frame layer

        self.first_arrived = true;

        match frame {
            Frame::Ping { payload } => {
                self.read
                    .sender
                    .send(Event::SendPongFrame(Frame::Pong { payload }))
                    .map_err(|_| WebSocketError::ChannelError)?;
                Poll::Pending
            }
            Frame::Pong { payload } => {
                self.read.wake_up_pongs(payload);
                Poll::Pending
            }

            Frame::Close { payload } => {
                self.read
                    .sender
                    .send(Event::SendCloseFrameAndShutdown(Frame::Close { payload }))
                    .map_err(|_| WebSocketError::ChannelError)?;
                Poll::Ready(None)
            }

            Frame::Binary { payload, fin, .. } => {
                // no need to check for bad continuation flags because its caught by the framing layer

                self.fin = fin;
                Poll::Ready(Some(Ok(MessageFragment::Binary(payload))))
            }
            Frame::Text {
                mut payload, fin, ..
            } => {
                // no need to check for bad continuation flags because its caught by the framing layer

                let prev_partial_utf8 = self.staggered_utf8.take();

                let current_partial_utf8 = match split_off_partial_utf8(&mut payload) {
                    Ok(v) => Some(v),
                    Err(SplitOffPartialError::NoPartialCodePoint) => None,
                    Err(SplitOffPartialError::InvalidUtf8) => {
                        return Poll::Ready(Some(Err(WebSocketError::InvalidFrame(
                            InvalidFrame::BadUtf8,
                        ))));
                    }
                };

                if let Some(prepend) = prev_partial_utf8 {
                    payload.splice(0..0, prepend.into_iter());
                }

                self.staggered_utf8 = current_partial_utf8;
                self.fin = fin;

                Poll::Ready(Some(Ok(MessageFragment::Text(
                    String::from_utf8(payload)
                        .map_err(|_| WebSocketError::InvalidFrame(InvalidFrame::BadUtf8))?,
                ))))
            }
        }
    }
}

impl<'a> FusedStream for FragmentedMessage<'a> {
    fn is_terminated(&self) -> bool {
        self.fin
    }
}

impl<'a> Drop for FragmentedMessage<'a> {
    fn drop(&mut self) {
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

    /// Send pong wake up handles
    pub(super) pong_sender: Sender<oneshot::Sender<PingPayload>>,
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

/// Events sent from the read half to the write half
#[derive(Debug)]
pub(super) enum Event {
    SendPongFrame(Frame),
    SendCloseFrameAndShutdown(Frame),
}

/// Splits off any partial utf8 characters from a string (!) buffer.
fn split_off_partial_utf8(vec: &mut Vec<u8>) -> Result<Vec<u8>, SplitOffPartialError> {
    let bytes_from_end = vec.iter().rev().copied();

    // number of bytes belonging to a potentially partial code point, found in the current buffer.
    let mut number_partial = 0usize;
    for byte in bytes_from_end {
        // if byte start of multi byte code point
        if (byte & 0b111 == 0b110) || (byte & 0b1111 == 0b1110) || (byte & 0b11111 == 0b11110) {
            // count result
            number_partial += 1;
            // stop iterating
            break;
        }

        // If partial code point abruptly stops or no multi byte code point exists in general
        if byte & 0b11 != 0b10 {
            return Err(if number_partial == 0 {
                // there were no continuation bytes.
                SplitOffPartialError::NoPartialCodePoint
            } else {
                // multi byte code point abruptly stops without a starting byte.
                SplitOffPartialError::InvalidUtf8
            });
        }

        number_partial += 1;
    }

    // get the length in bytes of the last code point
    let code_point_len: usize = {
        let byte = vec
            .iter()
            .skip(vec.len() - number_partial)
            .next()
            .unwrap_or_else(|| unreachable!());

        if byte & 0b11111 == 0b11110 {
            4
        } else if byte & 0b1111 == 0b1110 {
            3
        } else if byte & 0b111 == 0b110 {
            2
        } else {
            // no if statement for a validity check as it would have been weeded out by now.
            1
        }
    };

    // check if last code point collected is partial
    if code_point_len < number_partial {
        // More continuation bytes than the byte prescribes
        Err(SplitOffPartialError::InvalidUtf8)
    } else if code_point_len == number_partial {
        Err(SplitOffPartialError::NoPartialCodePoint)
    } else {
        // do the thing
        Ok(vec.split_off(vec.len() - number_partial))
    }
}

enum SplitOffPartialError {
    NoPartialCodePoint,
    InvalidUtf8,
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
