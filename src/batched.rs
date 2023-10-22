/*
 * Copyright © 2023 spiceswag
 * Licensed to the world under the MIT License.
 */

//! A useful utility sink for slowly feeding a vector of items into the wrapped sink.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Sink;

/// Wraps a sink and buffers it so it can accept a bulk of items in one start_send operation.
///
/// This is useful if you are implementing a wrapper around a sink that
/// for example fragments its item and forwards it to the next sink.
#[derive(Debug)]
pub struct Batched<Si, Item>
where
    Si: Sink<Item>,
{
    sink: Si,
    buf: Vec<Item>,
    bounds: Bounds,
}

/// Bounds to the expansion of the item buffer.
#[derive(Debug)]
enum Bounds {
    Bounded(usize),
    Unbounded,
}

impl<Si: Sink<Item>, Item> Sink<Item> for Batched<Si, Item> {
    type Error = Si::Error;

    fn start_send(self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
        todo!()
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        todo!()
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        todo!()
    }
}

impl<Si: Sink<Item>, Item> Sink<Vec<Item>> for Batched<Si, Item> {
    type Error = Si::Error;

    fn start_send(self: Pin<&mut Self>, item: Vec<Item>) -> Result<(), Self::Error> {
        todo!()
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        todo!()
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        todo!()
    }
}
