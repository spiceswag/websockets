//! Operations on websockets with custom future implementations.
//! This includes closing the websocket, and pinging the peer.

use futures::FutureExt;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::oneshot;

/// A named that resolves as soon as a
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
    recv: oneshot::Receiver<Option<Vec<u8>>>,
}

impl Future for Pong {
    type Output = Option<Vec<u8>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.recv
            .poll_unpin(cx)
            .map(|res| res.expect("Pong future polled after the relevant websocket is closed"))
    }
}

/// A future that resolves when the remote peer acknoledges
/// a `Close` frame or drops its send half of the socket.
///
/// # Data Loss
///
/// Data frames received while waiting for the close acknoledgement
/// packet from the server will be lost.
///
/// If this is unacceptable, opt for
///
/// https://datatracker.ietf.org/doc/html/rfc6455#section-7
#[derive(Debug)]
pub struct Close<'a> {
    todo: std::marker::PhantomData<&'a ()>,
}
