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

#[derive(Debug)]
pub struct MessageFragment {}

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
}

/// A list of fragmentation strategies provided by the library with the common purpose
/// of splitting up an already available whole [`Message`] into frames
///
/// **`Fragmentation::None`** is the strategy to use when you're sure you won't be tripping
/// the server's upper limit on frame size, such as when identifying to the discord gateway,
/// as at most your frame size will be 120 bytes.
///
/// **`Fragmentation::WithThreshold`** is appopriate for simple splitting of messages based on
/// a length threshold and a maximum frame size which the library will try to fill as many frames with.
#[derive(Debug)]
pub enum Fragmentation {
    None,
    WithThreshold {
        threshold: usize,
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
                    payload,
                    continuation: false,
                    fin: true,
                }],
            },
            Self::WithThreshold {
                threshold,
                max_frame_size,
            } => match message {
                Message::Binary(payload) => {
                    todo!()
                }
                Message::Text(payload) => {
                    todo!()
                }
            },
        }
    }
}
