use std::{pin::Pin, task::Poll};

use flume::{Receiver, Sender};
use futures::{ready, stream::FusedStream, Stream, StreamExt};
use tokio::{
    io::{BufReader, ReadHalf},
    sync::oneshot,
};
use tokio_util::codec::FramedRead;

use crate::{
    error::InvalidFrame,
    ops::PingPayload,
    websocket::{
        frame::{Frame, FrameType, WsFrameCodec},
        message::{IncompleteMessage, MessageFragment},
        transport::Transport,
    },
    Message, WebSocketError,
};

/// The read half of a WebSocket connection, generated from [`WebSocket::split()`].
/// This half can only receive frames.
#[derive(Debug)]
pub struct WebSocketReadHalf {
    stream: FramedRead<BufReader<ReadHalf<Transport>>, WsFrameCodec>,

    event_sender: Sender<Event>,

    /// A receiver for pong future handles.
    pong_receiver: Receiver<oneshot::Sender<PingPayload>>,
    /// A list of handles for waking up pong futures.
    pongs: Vec<oneshot::Sender<PingPayload>>,

    /// Part of a message that has not fully been received yet.
    /// Should be `None` if the stream is being managed by a [`FragmentedMessage`]
    partial_message: Option<IncompleteMessage>,

    /// If the stream is in the middle of receiving a fragmented message,
    /// but the FragmentedMessage struct was dropped.
    recovering: bool,
}

impl WebSocketReadHalf {
    pub(crate) fn new(
        stream: FramedRead<BufReader<ReadHalf<Transport>>, WsFrameCodec>,
        event_sender: Sender<Event>,
        pong_receiver: Receiver<oneshot::Sender<PingPayload>>,
    ) -> Self {
        Self {
            stream,
            event_sender,
            pong_receiver,
            pongs: vec![],
            partial_message: None,
            recovering: false,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        FramedRead<BufReader<ReadHalf<Transport>>, WsFrameCodec>,
        Option<IncompleteMessage>,
        bool,
    ) {
        let Self {
            stream,
            partial_message,
            recovering,
            ..
        } = self;
        (stream, partial_message, recovering)
    }

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

        if self.recovering {
            match &frame {
                Frame::Text { fin, .. } => {
                    if *fin {
                        self.recovering = false;
                    } else {
                        return Poll::Pending;
                    }
                }
                Frame::Binary { fin, .. } => {
                    if *fin {
                        self.recovering = false;
                    } else {
                        return Poll::Pending;
                    }
                }
                _ => {}
            }
        }

        match frame {
            Frame::Ping { payload } => {
                self.event_sender
                    .send(Event::SendPongFrame(Frame::Pong { payload }))
                    .map_err(|_| WebSocketError::ChannelError)?;

                Poll::Pending
            }
            Frame::Pong { payload } => {
                self.wake_up_pongs(payload);
                Poll::Pending
            }

            Frame::Close { payload } => {
                self.event_sender
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
///
/// # Data Loss
///
/// When dropping this structure, if the message is not finished,
/// the rest of that message's frames are lost.
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

        if let Some(partial_message) = self.read.partial_message.take() {
            return Poll::Ready(Some(Ok(match partial_message {
                IncompleteMessage::Binary(payload) => MessageFragment::Binary(payload),
                IncompleteMessage::Text(mut payload) => {
                    // self.staggered_utf8 must be None at this point

                    let current_partial_utf8 = match split_off_partial_utf8(&mut payload) {
                        Ok(v) => Some(v),
                        Err(SplitOffPartialError::NoPartialCodePoint) => None,
                        Err(SplitOffPartialError::InvalidUtf8) => {
                            return Poll::Ready(Some(Err(WebSocketError::InvalidFrame(
                                InvalidFrame::BadUtf8,
                            ))));
                        }
                    };

                    self.staggered_utf8 = current_partial_utf8;

                    MessageFragment::Text(
                        String::from_utf8(payload)
                            .map_err(|_| WebSocketError::InvalidFrame(InvalidFrame::BadUtf8))?,
                    )
                }
            })));
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
                    .event_sender
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
                    .event_sender
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
        if !self.fin {
            self.read.recovering = true;
        }
    }
}

/// Events sent from the read half to the write half
#[derive(Debug)]
pub(crate) enum Event {
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
