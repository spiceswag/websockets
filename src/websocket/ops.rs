//! Operations on web sockets with custom future implementations.
//! This includes closing the websocket, and pinging the peer.

use futures::{ready, FutureExt, Stream, StreamExt};
use std::{
    convert::{TryFrom, TryInto},
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{BufReader, ReadHalf},
    select,
    sync::oneshot,
    time::Sleep,
};
use tokio_util::codec::FramedRead;

use crate::{error::InvalidFrame, Message, WebSocketError, WebSocketReadHalf};

use super::{
    frame::{Frame, WsFrameCodec},
    message::IncompleteMessage,
    transport::Transport,
};

/// A futures that resolves as soon as a
/// pong message is received from the server.
///
/// Each pong future maps one-to-one with a ping frame sent
/// to the server, but not vice-versa, with respect to the ping sender.
///
/// The websocket spec states that servers peers may choose
/// to "aggregate" pong frames by only replying to the latest one.
/// Therefore, pong futures may resolve in waves,
/// which may produce a warped perception of the actual latency
/// between the websocket peers.
///
/// # Panics
///
/// Polling this future causes a panic if
/// the relevant websocket has been dropped or closed.
///
/// https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.3
#[derive(Debug)]
pub struct Pong {
    pub(super) recv: oneshot::Receiver<PingPayload>,
}

impl Future for Pong {
    type Output = Option<PingPayload>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.recv.poll_unpin(cx).map(|res| res.ok())
    }
}

/// The contents of a `Ping`/`Pong` frame
///
/// https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.3
#[derive(Debug)]
pub struct PingPayload {
    /// Optional application data contained in a `Ping`.
    pub payload: Option<Vec<u8>>,
}

/// Close a WebSocket while also catching the last frames sent by the server!
/// This is the data loss proof alternative to [`WebSocket::close`].
#[derive(Debug)]
pub struct ClosingFrames {
    read: FramedRead<BufReader<ReadHalf<Transport>>, WsFrameCodec>,
    /// Part of a message that has not fully been received yet.
    partial_message: Option<IncompleteMessage>,

    /// If the stream is in the middle of receiving a fragmented message,
    /// but the FragmentedMessage struct was dropped.
    recovering: bool,

    /// If the server has echoed a `Close` frame, this field is `Some`, `None` otherwise
    complete: Option<ClosePayload>,

    /// When the server should have closed the connection by.
    /// If not met, the client drops the TCP connection on its own.
    timeout: Pin<Box<Sleep>>,
}

impl ClosingFrames {
    pub(crate) fn new(read: WebSocketReadHalf) -> Self {
        let (read, partial_message, recovering) = read.into_parts();

        ClosingFrames {
            read,
            partial_message,
            recovering,
            complete: None,
            timeout: Box::pin(tokio::time::sleep(Duration::from_secs(5))),
        }
    }

    /// If the WebSocket has been closed, this method returns
    /// any leftover parts from the last data message, if it was not received in full.
    pub fn remainder(&mut self) -> Option<IncompleteMessage> {
        // if the connection is not done
        if self.complete.is_none() {
            return None;
        }

        self.partial_message.take()
    }

    /// Wait until the server echoes the `Close` frame and closes the TCP connection.
    ///
    /// # Data Loss
    ///
    /// Data send by the server which have not been received will be lost.
    /// If this is undesirable just read the [`Stream`] to its end.
    pub async fn close(self) -> Result<CloseOutcome, WebSocketError> {
        let Self {
            mut read,
            complete,
            timeout,
            ..
        } = self;

        if let Some(payload) = complete {
            return Ok(CloseOutcome::Normal(Some(payload)));
        }

        let wait_for_close = async move {
            loop {
                match read.next().await {
                    None => return Ok::<CloseOutcome, WebSocketError>(CloseOutcome::Abrupt),
                    Some(Err(err)) => return Err(err.0),
                    Some(Ok(frame)) => match frame {
                        Frame::Close { payload: None } => return Ok(CloseOutcome::Normal(None)),
                        Frame::Close {
                            payload: Some((status, reason)),
                        } => {
                            return Ok(CloseOutcome::Normal(Some(ClosePayload {
                                status: status.try_into()?,
                                reason: Some(reason).filter(|s| !s.is_empty()),
                            })))
                        }
                        _ => {}
                    },
                }
            }
        };

        select! {
            res = wait_for_close => res,
            _ = timeout => Ok(CloseOutcome::TimeOut)
        }
    }
}

impl Stream for ClosingFrames {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.complete.is_some() {
            return Poll::Ready(None);
        }

        // piece together a frame

        let frame = match ready!(self.read.poll_next_unpin(cx)) {
            Some(Ok(f)) => f,
            Some(Err(err)) => return Poll::Ready(Some(Err(err.0))),
            None => return Poll::Ready(None),
        };

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
            Frame::Close { payload } => {
                self.complete = Some(match payload {
                    Some((status, reason)) => ClosePayload {
                        status: Status::try_from(status)?,
                        reason: Some(reason).filter(|s| !s.is_empty()),
                    },
                    None => ClosePayload {
                        status: Status::MissingStatusCode,
                        reason: None,
                    },
                });
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
            _ => Poll::Pending,
        }
    }
}

/// How a WebSocket connection was closed.
#[derive(Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The WebSocket connection was closed normally.
    Normal(Option<ClosePayload>),
    /// The server took too long to reply with a `Close` frame,
    /// or took too long to shut down the TCP connection,
    /// so the client acted first.
    TimeOut,
    /// Abrupt closure of the connection without any echoed `Close` frame.
    Abrupt,
}

impl CloseOutcome {
    /// Unwrap the outcome assuming it was a normal closure, or panic otherwise.
    ///
    /// # Panics
    ///
    /// This method panics if `self` is equal to [`CloseOutcome::TimeOut`] or [`CloseOutcome::Abrupt`]
    pub fn unwrap(self) -> Option<ClosePayload> {
        match self {
            Self::Normal(p) => p,
            Self::TimeOut => panic!("unwrap called on a time out close outcome"),
            Self::Abrupt => panic!("unwrap called on a abrupt closure close outcome"),
        }
    }
}

/// The body of a close frame, consisting of a status code
/// hinting at the reason why the connection is being closed,
/// and a reason message meant to be read by the developers of
/// the application communicating.
///
/// https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.1
#[derive(Debug, PartialEq, Eq)]
pub struct ClosePayload {
    /// The closing status for this payload.
    pub status: Status,
    /// Reason given for closing the connection.
    pub reason: Option<String>,
}

/// Status codes observable during the closing of a WebSocket connection.
///
/// This enum has been idiot-proofed, for the lack of a better term.
///
/// That means you can not use this enum in combination with any other API of this crate
/// to cause an active WebSocket connection to enter an invalid state.
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// The purpose of the websocket has been fulfilled.
    ///
    /// WebSocket status code 1000
    Ok,

    /// An endpoint is leaving the connection,
    /// such as a browser navigating away or a web server shutting down.
    ///
    /// WebSocket status code 1001
    GoingAway,

    /// An endpoint is terminating the connection due to a protocol error.
    ///
    /// WebSocket status code 1002
    ProtocolError,

    /// An endpoint has received data of a type it doesn't understand.
    ///
    /// WebSocket status code 1003
    UnacceptableData,

    // 1004 reserved
    /// No actual status code is present.
    ///
    /// # Oddities
    ///
    /// This status code is never sent by a peer, but is generated instead
    /// from the `websockets` library itself, as per spec, to indicate
    /// what's on the label.
    ///
    /// This status code can not be sent manually.
    ///
    /// WebSocket status code 1005
    MissingStatusCode,

    /// Indicates that the connection was shut down abnormally,
    /// for example, if no `Close` frames were exchanged
    ///
    /// This status code may not be sent by a peer but instead,
    /// generated by the `websockets` library when the peer did not
    /// reply with a `Close` frame of their own during shutdown.
    ///
    /// This status code can not be sent manually.
    ///
    /// WebSocket status code 1006
    AbnormalShutdown,

    /// An endpoint received data that was inconsistent with the
    /// type of the message.
    ///
    /// WebSocket status code 1007
    InvalidData,

    /// An endpoint has received data within a message that violates its policy.
    ///
    /// WebSocket status code 1008
    PolicyViolation,

    /// An endpoint has received a message that was too big.
    ///
    /// WebSocket status code 1009
    MessageTooLarge,

    /// The client is terminating the connection because it
    /// expected the server to negotiate one or more extensions, but the server didn't.
    ///
    /// The list of extensions that are needed should appear in the `reason` part of the Close frame.
    ///
    /// WebSocket status code 1010
    InsufficientExtensions,

    /// The server is terminating the connection because it encountered
    /// an unexpected condition that prevented it from fulfilling the request.
    ///
    /// WebSocket status code 1011
    ServerError,

    /// This status code is designated for use in
    /// applications expecting a status code to indicate that the
    /// connection was closed due to a failure to perform a TLS handshake.
    ///
    /// This status code may never be observed, as its purpose is fulfilled
    /// by the [`WebSocketError::TlsConnectionError`] error.
    ///
    /// # Oddities
    ///
    /// This status code is never sent by a peer, but is generated instead
    /// from the `websockets` library itself, as per spec, to indicate
    /// what's on the label.
    ///
    /// This status code can not be sent manually.
    ///
    /// WebSocket status code 1015
    TlsFailure,

    /// Status codes in the range `3000..=3999` are reserved for use by
    /// libraries, frameworks, and applications.  These status codes are
    /// registered directly with IANA.
    RegisteredStatus(u16),

    /// Status codes in the range `4000..=4999` are reserved for private use.
    PrivateStatus(u16),
}

impl Status {
    /// If the status code can be sent through the WebSocket.
    pub fn sendable(&self) -> bool {
        match self {
            Self::MissingStatusCode => false,
            Self::AbnormalShutdown => false,
            Self::TlsFailure => false,
            _ => true,
        }
    }
}

impl TryFrom<u16> for Status {
    type Error = WebSocketError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0..=999 => Err(WebSocketError::InvalidFrame(InvalidFrame::BadCloseCode)),

            1000 => Ok(Self::Ok),
            1001 => Ok(Self::GoingAway),
            1002 => Ok(Self::ProtocolError),
            1003 => Ok(Self::UnacceptableData),
            // 1004 reserved
            1005 => Ok(Self::MissingStatusCode),
            1006 => Ok(Self::AbnormalShutdown),

            1007 => Ok(Self::InvalidData),
            1008 => Ok(Self::PolicyViolation),
            1009 => Ok(Self::MessageTooLarge),
            1010 => Ok(Self::InsufficientExtensions),
            1011 => Ok(Self::ServerError),

            1015 => Ok(Self::TlsFailure),

            3000..=3999 => Ok(Self::RegisteredStatus(value)),
            4000..=4999 => Ok(Self::PrivateStatus(value)),

            _ => Err(WebSocketError::InvalidFrame(InvalidFrame::BadCloseCode)),
        }
    }
}

impl Into<u16> for Status {
    fn into(self) -> u16 {
        use Status::*;

        match self {
            Ok => 1000,
            GoingAway => 1001,
            ProtocolError => 1002,
            UnacceptableData => 1003,
            MissingStatusCode => 1005,
            AbnormalShutdown => 1006,
            InvalidData => 1007,
            PolicyViolation => 1008,
            MessageTooLarge => 1009,
            InsufficientExtensions => 1010,
            ServerError => 1011,
            TlsFailure => 1015,
            RegisteredStatus(v) => v,
            PrivateStatus(v) => v,
        }
    }
}
