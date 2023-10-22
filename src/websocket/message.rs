//! Websocket messages are the logical unit that developers interact with.

use crate::Frame;

#[derive(Debug)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
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
        todo!()
    }
}
