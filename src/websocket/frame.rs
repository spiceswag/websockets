use std::convert::TryInto;

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

#[allow(unused_imports)] // for intra doc links
use super::WebSocket;
use crate::error::{WebSocketError, WsReadError, WsWriteError};

const U16_MAX_MINUS_ONE: usize = (u16::MAX - 1) as usize;
const U16_MAX: usize = u16::MAX as usize;
const U64_MAX_MINUS_ONE: usize = (u64::MAX - 1) as usize;

/// Max payload size in bytes
const MAX_PAYLOAD_SIZE: usize = 2_000_000; // 2 MB

/// Send and receive WebSocket frames over the beloved [`Stream`] and [`Sink`] traits.
///
/// [`Stream`]: futures::Stream
/// [`Sink`]: futures::Sink
#[derive(Debug)]
pub(crate) struct WsFrameCodec {
    // receive
    last_frame_type: Option<FrameType>,
    // send
    rng: ChaCha20Rng,
}

impl WsFrameCodec {
    /// Create a WebSocket frame decoder.
    pub fn new() -> Self {
        Self {
            last_frame_type: None,
            rng: ChaCha20Rng::from_entropy(),
        }
    }
}

impl Decoder for WsFrameCodec {
    type Item = Frame;
    type Error = WsReadError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        use std::io::Cursor;
        if src.len() < 2 {
            // 16 bits
            return Ok(None);
        }

        let mut src = Cursor::new(src);

        fn read_u8<T: std::io::Read>(t: &mut T) -> Result<u8, std::io::Error> {
            let mut buf = [0; 1];
            t.read(&mut buf)?;
            Ok(buf[0])
        }

        let (fin, opcode) = {
            let byte = read_u8(&mut src).unwrap();

            let fin = byte & 0b10000000_u8 != 0;
            let opcode = byte & 0b00001111_u8;

            (fin, opcode)
        };
        let (masked, length_specifier) = {
            let byte = read_u8(&mut src).unwrap();

            let masked = byte & 0b10000000_u8 != 0;
            let length = (byte & 0b01111111_u8) << 1;

            (masked, length)
        };

        if masked {
            return Err(WsReadError(WebSocketError::ReceivedMaskedFrameError));
        }

        let payload_length = match length_specifier {
            0..=125 => length_specifier as usize,
            126 => {
                if src.get_ref().len() < 4 {
                    // 32 bits
                    return Ok(None);
                }

                let mut buf = [0; 2];
                std::io::Read::read(&mut src, &mut buf).unwrap();
                u16::from_be_bytes(buf) as usize
            }
            127 => {
                if src.get_ref().len() < 10 {
                    // 80 bits
                    return Ok(None);
                }

                let mut buf = [0; 8];
                std::io::Read::read(&mut src, &mut buf).unwrap();
                u64::from_be_bytes(buf) as usize
            }
            _ => return Err(WsReadError(WebSocketError::InvalidFrameError)),
        };

        if payload_length > MAX_PAYLOAD_SIZE {
            return Err(WsReadError(WebSocketError::PayloadTooLargeError));
        }

        if src.get_ref().len() < src.position() as usize + payload_length {
            return Ok(None);
        }

        let mut payload = vec![0u8; payload_length];
        std::io::Read::read(&mut src, &mut payload).unwrap();

        let consumed = src.position() as usize;
        let _ = src.into_inner().split_to(consumed);

        match opcode {
            // continuation frame
            0x0 => match self.last_frame_type {
                Some(FrameType::Text) => {
                    if fin {
                        self.last_frame_type = None;
                    }

                    Ok(Some(Frame::Text {
                        payload,
                        continuation: true,
                        fin,
                    }))
                }
                Some(FrameType::Binary) => {
                    if fin {
                        self.last_frame_type = None;
                    }

                    Ok(Some(Frame::Binary {
                        payload,
                        continuation: true,
                        fin,
                    }))
                }
                _ => Err(WsReadError(WebSocketError::InvalidFrameError)),
            },
            0x1 => {
                if !fin {
                    self.last_frame_type = Some(FrameType::Text);
                }

                Ok(Some(Frame::Text {
                    payload,
                    continuation: false,
                    fin,
                }))
            }
            0x2 => {
                if !fin {
                    self.last_frame_type = Some(FrameType::Binary);
                }

                Ok(Some(Frame::Binary {
                    payload,
                    continuation: false,
                    fin,
                }))
            }
            // reserved data frames
            0x3..=0x7 => Err(WsReadError(WebSocketError::InvalidFrameError)),
            0x8 if payload_length == 0 => Ok(Some(Frame::Close { payload: None })),
            // if there is a payload it must have a u16 status code
            0x8 if payload_length < 2 => Err(WsReadError(WebSocketError::InvalidFrameError)),
            0x8 => {
                let (status_code, reason) = payload.split_at(2);
                let status_code = u16::from_be_bytes(
                    status_code
                        .try_into()
                        .map_err(|_e| WsReadError(WebSocketError::InvalidFrameError))?,
                );
                Ok(Some(Frame::Close {
                    payload: Some((
                        status_code,
                        String::from_utf8(reason.to_vec())
                            .map_err(|_e| WsReadError(WebSocketError::InvalidFrameError))?,
                    )),
                }))
            }
            0x9 if payload_length == 0 => Ok(Some(Frame::Ping { payload: None })),
            0x9 => Ok(Some(Frame::Ping {
                payload: Some(payload),
            })),
            0xA if payload_length == 0 => Ok(Some(Frame::Pong { payload: None })),
            0xA => Ok(Some(Frame::Pong {
                payload: Some(payload),
            })),
            // reserved control frames
            0xB..=0xFF => return Err(WsReadError(WebSocketError::InvalidFrameError)),
        }
    }
}

impl Encoder<Frame> for WsFrameCodec {
    type Error = WsWriteError;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let is_control = item.is_control();
        let opcode = item.opcode();
        let fin = item.fin();

        let mut payload = match item {
            // https://tools.ietf.org/html/rfc6455#section-5.6
            Frame::Text { payload, .. } => payload,
            Frame::Binary { payload, .. } => payload,
            // https://tools.ietf.org/html/rfc6455#section-5.5.1
            Frame::Close {
                payload: Some((status_code, reason)),
            } => {
                let mut payload = status_code.to_be_bytes().to_vec();
                payload.append(&mut reason.into_bytes());
                payload
            }
            Frame::Close { payload: None } => Vec::new(),
            // https://tools.ietf.org/html/rfc6455#section-5.5.2
            Frame::Ping { payload } => payload.unwrap_or(Vec::new()),
            // https://tools.ietf.org/html/rfc6455#section-5.5.3
            Frame::Pong { payload } => payload.unwrap_or(Vec::new()),
        };
        // control frame cannot be longer than 125 bytes: https://tools.ietf.org/html/rfc6455#section-5.5
        if is_control && payload.len() > 125 {
            return Err(WsWriteError(WebSocketError::ControlFrameTooLargeError));
        }

        // set payload len: https://tools.ietf.org/html/rfc6455#section-5.2
        dst.reserve(14 + payload.len());

        dst.extend_from_slice(&[opcode + fin]);
        let mut payload_len_data = match payload.len() {
            0..=125 => (payload.len() as u8).to_be_bytes().to_vec(),
            126..=U16_MAX_MINUS_ONE => {
                let mut payload_len_data = vec![126];
                payload_len_data.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                payload_len_data
            }
            U16_MAX..=U64_MAX_MINUS_ONE => {
                let mut payload_len_data = vec![127];
                payload_len_data.extend_from_slice(&(payload.len() as u64).to_be_bytes());
                payload_len_data
            }
            _ => return Err(WsWriteError(WebSocketError::PayloadTooLargeError)),
        };
        payload_len_data[0] += 0b10000000; // set masking bit: https://tools.ietf.org/html/rfc6455#section-5.3
        dst.extend_from_slice(&payload_len_data);

        // payload masking: https://tools.ietf.org/html/rfc6455#section-5.3
        let mut masking_key = vec![0; 4];
        self.rng.fill_bytes(&mut masking_key);
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = *byte ^ (masking_key[i % 4]);
        }
        dst.extend_from_slice(&masking_key);

        dst.extend_from_slice(&payload);

        Ok(())
    }
}

// https://tools.ietf.org/html/rfc6455#section-5.2
/// Data which is sent and received through the WebSocket connection.
///
/// Sending and receiving raw frames is inaccessible in the public API,
/// due to protocol integrity concerns, send messages or fragments instead.
///
/// # Fragmentation
///
/// As per the WebSocket protocol, frames can actually be fragments in a larger message
/// (see [https://tools.ietf.org/html/rfc6455#section-5.4](https://tools.ietf.org/html/rfc6455#section-5.4)).
/// However, the maximum frame size allowed by the WebSocket protocol is larger
/// than what can be stored in a `Vec`. Therefore, no strategy for splitting messages
/// into Frames is provided by this library.
///
/// If you would like to use fragmentation manually, this can be done by setting
/// the `continuation` and `fin` flags on the `Text` and `Binary` variants.
/// `continuation` signifies that the Frame is a Continuation frame in the message,
/// and `fin` signifies that the Frame is the final frame in the message
/// (see the above linked RFC for more details).
///
/// For example, if the message contains only one Frame, the single frame
/// should have `continuation` set to `false` and `fin` set to `true`. If the message
/// contains more than one frame, the first frame should have `continuation` set to
/// `false` and `fin` set to `false`, all other frames except the last frame should
/// have `continuation` set to `true` and `fin` set to `false`, and the last frame should
/// have `continuation` set to `true` and `fin` set to `true`.
#[derive(Debug, Clone)]
pub enum Frame {
    /// A Text frame
    Text {
        /// The payload for the Text frame
        payload: Vec<u8>,
        /// Whether the Text frame is a continuation frame in the message
        continuation: bool,
        /// Whether the Text frame is the final frame in the message
        fin: bool,
    },
    /// A Binary frame
    Binary {
        /// The payload for the Binary frame
        payload: Vec<u8>,
        /// Whether the Binary frame is a continuation frame in the message
        continuation: bool,
        /// Whether the Binary frame is the final frame in the message
        fin: bool,
    },
    /// A Close frame
    Close {
        /// The payload for the Close frame
        payload: Option<(u16, String)>,
    },
    /// A Ping frame
    Ping {
        /// The payload for the Ping frame
        payload: Option<Vec<u8>>,
    },
    /// A Pong frame
    Pong {
        /// The payload for the Pong frame
        payload: Option<Vec<u8>>,
    },
}

impl Frame {
    /// Constructs a Text frame from the given payload.
    /// `continuation` will be `false` and `fin` will be `true`.
    /// This can be modified by chaining [`Frame::set_continuation()`] or [`Frame::set_fin()`].
    pub fn text(payload: String) -> Self {
        Self::Text {
            payload: payload.into_bytes(),
            continuation: false,
            fin: true,
        }
    }

    /// Returns whether the frame is a Text frame.
    pub fn is_text(&self) -> bool {
        self.as_text().is_some()
    }

    /// Attempts to interpret the frame as a Text frame,
    /// returning a reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_text(&self) -> Option<(&[u8], &bool, &bool)> {
        match self {
            Self::Text {
                payload,
                continuation,
                fin,
            } => Some((payload, continuation, fin)),
            _ => None,
        }
    }
    /// Attempts to interpret the frame as a Text frame,
    /// returning a mutable reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_text_mut(&mut self) -> Option<(&mut Vec<u8>, &mut bool, &mut bool)> {
        match self {
            Self::Text {
                payload,
                continuation,
                fin,
            } => Some((payload, continuation, fin)),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Text frame,
    /// consuming and returning the underlying data if it is,
    /// and returning None otherwise.
    pub fn into_text(self) -> Option<(Vec<u8>, bool, bool)> {
        match self {
            Self::Text {
                payload,
                continuation,
                fin,
            } => Some((payload, continuation, fin)),
            _ => None,
        }
    }

    /// Constructs a Binary frame from the given payload.
    /// `continuation` will be `false` and `fin` will be `true`.
    /// This can be modified by chaining [`Frame::set_continuation()`] or [`Frame::set_fin()`].
    pub fn binary(payload: Vec<u8>) -> Self {
        Self::Binary {
            payload,
            continuation: false,
            fin: true,
        }
    }

    /// Returns whether the frame is a Binary frame.
    pub fn is_binary(&self) -> bool {
        self.as_binary().is_some()
    }

    /// Attempts to interpret the frame as a Binary frame,
    /// returning a reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_binary(&self) -> Option<(&Vec<u8>, &bool, &bool)> {
        match self {
            Self::Binary {
                payload,
                continuation,
                fin,
            } => Some((payload, continuation, fin)),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Binary frame,
    /// returning a mutable reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_binary_mut(&mut self) -> Option<(&mut Vec<u8>, &mut bool, &mut bool)> {
        match self {
            Self::Binary {
                payload,
                continuation,
                fin,
            } => Some((payload, continuation, fin)),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Binary frame,
    /// consuming and returning the underlying data if it is,
    /// and returning None otherwise.
    pub fn into_binary(self) -> Option<(Vec<u8>, bool, bool)> {
        match self {
            Self::Binary {
                payload,
                continuation,
                fin,
            } => Some((payload, continuation, fin)),
            _ => None,
        }
    }

    /// Constructs a Close frame from the given payload.
    pub fn close(payload: Option<(u16, String)>) -> Self {
        Self::Close { payload }
    }

    /// Returns whether the frame is a Close frame.
    pub fn is_close(&self) -> bool {
        self.as_close().is_some()
    }

    /// Attempts to interpret the frame as a Close frame,
    /// returning a reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_close(&self) -> Option<&(u16, String)> {
        match self {
            Self::Close { payload } => payload.as_ref(),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Close frame,
    /// returning a mutable reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_close_mut(&mut self) -> Option<&mut (u16, String)> {
        match self {
            Self::Close { payload } => payload.as_mut(),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Close frame,
    /// consuming and returning the underlying data if it is,
    /// and returning None otherwise.
    pub fn into_close(self) -> Option<(u16, String)> {
        match self {
            Self::Close { payload } => payload,
            _ => None,
        }
    }

    /// Constructs a Ping frame from the given payload.
    pub fn ping(payload: Option<Vec<u8>>) -> Self {
        Self::Ping { payload }
    }

    /// Returns whether the frame is a Ping frame.
    pub fn is_ping(&self) -> bool {
        self.as_ping().is_some()
    }

    /// Attempts to interpret the frame as a Ping frame,
    /// returning a reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_ping(&self) -> Option<&Vec<u8>> {
        match self {
            Self::Ping { payload } => payload.as_ref(),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Ping frame,
    /// returning a mutable reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_ping_mut(&mut self) -> Option<&mut Vec<u8>> {
        match self {
            Self::Ping { payload } => payload.as_mut(),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Ping frame,
    /// consuming and returning the underlying data if it is,
    /// and returning  None otherwise.
    pub fn into_ping(self) -> Option<Vec<u8>> {
        match self {
            Self::Ping { payload } => payload,
            _ => None,
        }
    }

    /// Constructs a Pong frame from the given payload.
    pub fn pong(payload: Option<Vec<u8>>) -> Self {
        Self::Pong { payload }
    }

    /// Returns whether the frame is a Pong frame.
    pub fn is_pong(&self) -> bool {
        self.as_pong().is_some()
    }

    /// Attempts to interpret the frame as a Pong frame,
    /// returning a reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_pong(&self) -> Option<&Vec<u8>> {
        match self {
            Self::Pong { payload } => payload.as_ref(),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Pong frame,
    /// returning a mutable reference to the underlying data if it is,
    /// and None otherwise.
    pub fn as_pong_mut(&mut self) -> Option<&mut Vec<u8>> {
        match self {
            Self::Pong { payload } => payload.as_mut(),
            _ => None,
        }
    }

    /// Attempts to interpret the frame as a Pong frame,
    /// consuming and returning the underlying data if it is,
    /// and returning None otherwise.
    pub fn into_pong(self) -> Option<Vec<u8>> {
        match self {
            Self::Pong { payload } => payload,
            _ => None,
        }
    }

    /// Modifies the frame to set `continuation` to the desired value.
    /// If the frame is not a Text or Binary frame, no operation is performed.
    pub fn set_continuation(self, continuation: bool) -> Self {
        match self {
            Self::Text { payload, fin, .. } => Self::Text {
                payload,
                continuation,
                fin,
            },
            Self::Binary { payload, fin, .. } => Self::Binary {
                payload,
                continuation,
                fin,
            },
            _ => self,
        }
    }

    /// Modifies the frame to set `fin` to the desired value.
    /// If the frame is not a Text or Binary frame, no operation is performed.
    pub fn set_fin(self, fin: bool) -> Self {
        match self {
            Self::Text {
                payload,
                continuation,
                ..
            } => Self::Text {
                payload,
                continuation,
                fin,
            },
            Self::Binary {
                payload,
                continuation,
                ..
            } => Self::Binary {
                payload,
                continuation,
                fin,
            },
            _ => self,
        }
    }

    fn is_control(&self) -> bool {
        // control frames: https://tools.ietf.org/html/rfc6455#section-5.5
        match self {
            Self::Text { .. } | Self::Binary { .. } => false,
            _ => true,
        }
    }

    fn opcode(&self) -> u8 {
        // opcodes: https://tools.ietf.org/html/rfc6455#section-5.2
        match self {
            Self::Text { continuation, .. } => {
                if *continuation {
                    0x0
                } else {
                    0x1
                }
            }
            Self::Binary { continuation, .. } => {
                if *continuation {
                    0x0
                } else {
                    0x2
                }
            }
            Self::Close { .. } => 0x8,
            Self::Ping { .. } => 0x9,
            Self::Pong { .. } => 0xA,
        }
    }

    fn fin(&self) -> u8 {
        // fin bit: https://tools.ietf.org/html/rfc6455#section-5.2
        match self {
            Self::Text { fin, .. } => (*fin as u8) << 7,
            Self::Binary { fin, .. } => (*fin as u8) << 7,
            Self::Close { .. } => 0b10000000,
            Self::Ping { .. } => 0b10000000,
            Self::Pong { .. } => 0b10000000,
        }
    }

    pub fn frame_type(&self) -> FrameType {
        match self {
            Self::Text { .. } => FrameType::Text,
            Self::Binary { .. } => FrameType::Binary,
            _ => FrameType::Control,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameType {
    Text,
    Binary,
    Control,
}

impl Default for FrameType {
    fn default() -> Self {
        Self::Control
    }
}
