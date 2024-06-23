//! Websocket messages are the logical unit that developers interact with.

use super::frame::Frame;

/// A message is a logical unit of data, and usually carries an entire meaning by itself.
/// Over the wire, it can be split into frames.
///
/// For more information about fragmentation visit the [`Fragmentation`] strategy enum.
#[derive(Debug)]
pub enum Message {
    /// A UTF-8 formatted string message.
    ///
    /// https://datatracker.ietf.org/doc/html/rfc6455#section-5.6
    Text(String),
    /// A binary message, with no restrictions on formatting.
    ///
    /// https://datatracker.ietf.org/doc/html/rfc6455#section-5.6
    Binary(Vec<u8>),
}

impl Message {
    /// Try to interpret this message as a text message, returning `None` if it is not.
    pub fn as_text(self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Try to interpret this message as a binary message, returning `None` if it is not.
    pub fn as_binary(self) -> Option<Vec<u8>> {
        match self {
            Self::Binary(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Returns the length of the stored application data in bytes
    pub fn len(&self) -> usize {
        match self {
            Self::Text(string) => string.len(),
            Self::Binary(buf) => buf.len(),
        }
    }
}

/// A list of fragmentation strategies provided by the library with the common purpose
/// of splitting up an already available whole [`Message`] into frames
#[derive(Debug)]
pub enum Fragmentation {
    /// This is the strategy to use when you're sure you won't be tripping the server's upper limit
    /// on frame size, such as when identifying to the discord gateway, as at most your frame size will be 120 bytes.
    None,
    /// This strategy is appropriate for simple splitting of messages based on a length threshold
    /// and a maximum frame size which the library will try to fill as many frames with.
    WithThreshold {
        /// The message size threshold over which the algorithm will kick into effect.
        /// This value is independent of the `max_frame_size` below.
        threshold: usize,
        /// Once the above threshold is reached, this is the frame size the splitting algorithm will target.
        max_frame_size: usize,
    },
}

impl Fragmentation {
    /// Fragment a message in accordance to the selected parameters.
    pub fn fragment(&self, message: Message) -> Vec<Frame> {
        match self {
            Self::None => match message {
                Message::Binary(payload) => vec![Frame::Binary {
                    payload,
                    continuation: false,
                    fin: true,
                }],
                Message::Text(payload) => vec![Frame::Text {
                    payload: payload.into_bytes(),
                    continuation: false,
                    fin: true,
                }],
            },

            Self::WithThreshold { threshold, .. } if *threshold > message.len() => match message {
                Message::Binary(payload) => vec![Frame::Binary {
                    payload,
                    continuation: false,
                    fin: true,
                }],
                Message::Text(payload) => vec![Frame::Text {
                    payload: payload.into_bytes(),
                    continuation: false,
                    fin: true,
                }],
            },

            // conditions are not placed here because the compiler is stupid 😭😭😭
            Self::WithThreshold { max_frame_size, .. } => match message {
                Message::Binary(payload) => {
                    let mut iter = payload.chunks_exact(*max_frame_size);
                    let mut frames: Vec<Frame> = vec![];
                    frames.reserve(
                        iter.size_hint()
                            .1
                            .unwrap_or(iter.size_hint().0)
                            .saturating_add(1),
                    );

                    let mut is_first = true;

                    for chunk in &mut iter {
                        frames.push(Frame::Binary {
                            payload: chunk.to_owned(),
                            continuation: !is_first,
                            fin: false,
                        });
                        is_first = false;
                    }

                    // create a stubby half frame from the remainder
                    let remainder = iter.remainder();
                    if !remainder.is_empty() {
                        frames.push(Frame::Binary {
                            payload: remainder.to_owned(),
                            continuation: !is_first,
                            fin: true,
                        });
                    }

                    // flag last frame as last
                    frames.last_mut().map(|last_frame| match last_frame {
                        Frame::Binary { fin, .. } => *fin = true,
                        _ => {}
                    });

                    frames
                }
                Message::Text(payload) => {
                    let bytes = payload.into_bytes();
                    let mut iter = bytes.chunks_exact(*max_frame_size);
                    let mut frames: Vec<Frame> = vec![];
                    frames.reserve(
                        iter.size_hint()
                            .1
                            .unwrap_or(iter.size_hint().0)
                            .saturating_add(1),
                    );

                    let mut is_first = true;

                    for chunk in &mut iter {
                        frames.push(Frame::Binary {
                            payload: chunk.to_owned(),
                            continuation: !is_first,
                            fin: false,
                        });
                        is_first = false;
                    }

                    // create a stubby half frame from the remainder
                    let remainder = iter.remainder();
                    if !remainder.is_empty() {
                        frames.push(Frame::Text {
                            payload: remainder.to_owned(),
                            continuation: !is_first,
                            fin: true,
                        });
                    }

                    // flag last frame as last
                    frames.last_mut().map(|last_frame| match last_frame {
                        Frame::Text { fin, .. } => *fin = true,
                        _ => {}
                    });

                    frames
                }
            },
        }
    }
}

/// A message fragment is a part of a message.
///
/// Message fragments map one-to-one to data frames,
/// although the same is not true about the
/// application data in those frames
#[derive(Debug)]
pub enum MessageFragment {
    /// A UTF-8 formatted string message.
    ///
    /// # Split UTF-8
    ///
    /// In the websocket protocol, peers are allowed to split UTF-8 scalars across frame boundaries.
    ///
    /// When such a frame with split unicode scalars is received, the bytes of the scalar at the end of the frame are staggered
    /// and appear at the start of the next frame, in order to keep string correctness guarantees.
    ///
    /// https://datatracker.ietf.org/doc/html/rfc6455#section-5.6
    Text(String),
    /// A binary message, with no restrictions on formatting.
    ///
    /// https://datatracker.ietf.org/doc/html/rfc6455#section-5.6
    Binary(Vec<u8>),
}

/// A message in the middle of reconstruction.
#[derive(Debug)]
pub enum IncompleteMessage {
    Text(Vec<u8>),
    Binary(Vec<u8>),
}
