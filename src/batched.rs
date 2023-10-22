/*
 * Copyright © 2023 spiceswag
 * Licensed to the world under the MIT License.
 */

#![allow(dead_code)]

//! A useful utility sink for slowly feeding a vector of items into the wrapped sink.

use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{ready, Sink, SinkExt};

/// Wraps a sink and buffers it so it can accept a bulk of items in one start_send operation.
///
/// This is useful if you are implementing a wrapper around a sink that
/// for example fragments its item and forwards it to the next sink.
#[derive(Debug)]
pub struct Batched<Si, Item>
where
    Si: Sink<Item> + Unpin,
    Item: Unpin,
{
    sink: Si,
    buf: VecDeque<Item>,
}

impl<Si: Sink<Item> + Unpin, Item: Unpin> Batched<Si, Item> {
    /// Wrap a sink and give it the ability to accept items in batches.
    pub fn new(sink: Si, starting_size: usize) -> Self {
        let mut buf = VecDeque::new();
        buf.reserve(starting_size);

        Self { sink, buf }
    }

    /// Access the wrapped sink immutably.
    pub fn get_ref(&self) -> &Si {
        &self.sink
    }

    /// Access the wrapped sink mutably.
    pub fn get_mut(&mut self) -> &mut Si {
        &mut self.sink
    }

    /// Deflate the allocation of the buffer to a target size.
    /// The actual capacity after this function is either the target size, or the length of the buffer, whichever is largest.
    pub fn deflate_buffer(&mut self, target_size: usize) {
        self.buf.shrink_to(target_size)
    }

    fn try_flush_buffer(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Si::Error>> {
        ready!(self.sink.poll_ready_unpin(cx))?;
        while let Some(item) = self.buf.pop_front() {
            self.sink.start_send_unpin(item)?;
            if !self.buf.is_empty() {
                ready!(self.sink.poll_ready_unpin(cx))?;
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<Si, Item> Sink<Item> for Batched<Si, Item>
where
    Si: Sink<Item> + Unpin,
    Item: Unpin,
{
    type Error = Si::Error;

    fn start_send(mut self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
        if self.buf.len() == 0 {
            self.sink.start_send_unpin(item)
        } else {
            self.buf.push_back(item);
            Ok(())
        }
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.buf.len() == 0 {
            return self.sink.poll_ready_unpin(cx);
        }

        let _ = self.as_mut().try_flush_buffer(cx)?;

        if self.buf.len() >= self.buf.capacity() {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().try_flush_buffer(cx))?;
        debug_assert!(self.buf.is_empty());
        self.sink.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().try_flush_buffer(cx))?;
        debug_assert!(self.buf.is_empty());
        self.sink.poll_close_unpin(cx)
    }
}

impl<Si: Sink<Item> + Unpin, Item: Unpin> Sink<Prioritized<Item>> for Batched<Si, Item> {
    type Error = Si::Error;

    fn start_send(mut self: Pin<&mut Self>, item: Prioritized<Item>) -> Result<(), Self::Error> {
        if self.buf.len() == 0 {
            self.sink.start_send_unpin(item.0)
        } else {
            self.buf.push_front(item.0);
            Ok(())
        }
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.buf.len() == 0 {
            return self.sink.poll_ready_unpin(cx);
        }

        let _ = self.as_mut().try_flush_buffer(cx)?;

        if self.buf.len() >= self.buf.capacity() {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().try_flush_buffer(cx))?;
        debug_assert!(self.buf.is_empty());
        self.sink.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().try_flush_buffer(cx))?;
        debug_assert!(self.buf.is_empty());
        self.sink.poll_close_unpin(cx)
    }
}

/// Add an item to the front of the flush buffer of a [`Batched`] sink.
pub struct Prioritized<Item>(pub Item);

impl<Si: Sink<Item> + Unpin, Item: Unpin> Sink<Vec<Item>> for Batched<Si, Item> {
    type Error = Si::Error;

    fn start_send(mut self: Pin<&mut Self>, items: Vec<Item>) -> Result<(), Self::Error> {
        let mut items = items.into_iter();

        if self.buf.len() == 0 {
            if let Some(first) = items.next() {
                self.sink.start_send_unpin(first)?;
            }
        }

        self.buf.extend(items);
        Ok(())
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = &mut *self;

        if this.buf.len() == 0 {
            return self.sink.poll_ready_unpin(cx);
        }

        let _ = Pin::new(this).try_flush_buffer(cx)?;

        if self.buf.len() >= self.buf.capacity() {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().try_flush_buffer(cx))?;
        debug_assert!(self.buf.is_empty());
        self.sink.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().try_flush_buffer(cx))?;
        debug_assert!(self.buf.is_empty());
        self.sink.poll_close_unpin(cx)
    }
}
