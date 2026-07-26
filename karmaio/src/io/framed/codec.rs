use std::io;

use crate::buf::{IoBuf, IoBufMut, IoBufMutExt, Slice};

/// Encodes typed values into an owned buffer (payload only; framing is separate).
///
/// The buffer is expected to be cleared by the caller before `encode`. On success,
/// all initialized bytes are treated as payload content for [`super::frame::Framer::enclose`].
pub trait Encoder<Item, B: IoBufMut> {
    /// Error type returned during encoding. Must be constructible from I/O errors
    /// so framed write paths can unify codec and I/O failures.
    type Error: From<io::Error>;

    /// Encode `item` into `buf` as raw payload bytes (no framing).
    fn encode(&mut self, item: Item, buf: &mut B) -> Result<(), Self::Error>;
}

/// Decodes a complete payload view into a typed value.
///
/// The slice is the frame payload only
/// (prefix/suffix framing already stripped by the [`super::frame::Framer`]).
pub trait Decoder<B: IoBuf> {
    /// The type of decoded frames.
    type Item;

    /// Error type returned during decoding.
    type Error: From<io::Error>;

    /// Decode one complete payload.
    fn decode(&mut self, buf: &Slice<B>) -> Result<Self::Item, Self::Error>;

    /// Called when the underlying stream has reached EOF and residual bytes remain
    /// (or after all complete frames have been drained).
    ///
    /// The default implementation returns `Ok(None)` if `buf` is empty, otherwise
    /// decodes once via [`Decoder::decode`]. Override when finalization frames must
    /// differ from mid-stream decoding.
    fn decode_eof(&mut self, buf: &Slice<B>) -> Result<Option<Self::Item>, Self::Error> {
        if buf.is_empty() {
            Ok(None)
        } else {
            self.decode(buf).map(Some)
        }
    }
}

/// A trivial codec that treats frame payloads as raw byte vectors.
///
/// Useful for length-delimited or line-framed binary/text transport without an
/// additional serialization layer.
#[derive(Debug, Default, Clone, Copy)]
pub struct BytesCodec;

impl BytesCodec {
    /// Creates a new [`BytesCodec`].
    #[inline]
    pub fn new() -> Self {
        Self
    }
}

impl<B: IoBufMut + IoBufMutExt> Encoder<Vec<u8>, B> for BytesCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Vec<u8>, buf: &mut B) -> Result<(), Self::Error> {
        buf.extend_from_slice(&item);
        Ok(())
    }
}

impl<B: IoBuf> Decoder<B> for BytesCodec {
    type Item = Vec<u8>;
    type Error = io::Error;

    fn decode(&mut self, buf: &Slice<B>) -> Result<Self::Item, Self::Error> {
        Ok(buf.to_vec())
    }
}
