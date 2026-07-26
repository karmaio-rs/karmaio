use std::io;

use crate::buf::{BoundedIoBuf, IoBufMut, IoBufMutExt, Slice};

/// An extracted frame describing where the payload sits inside a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    // Offset where the frame payload begins.
    prefix: usize,
    // Length of the frame payload.
    payload: usize,
    // Suffix length after the payload (e.g. delimiter).
    suffix: usize,
}

impl Frame {
    /// Create a new [`Frame`] with the specified prefix, payload, and suffix lengths.
    #[inline]
    pub fn new(prefix: usize, payload: usize, suffix: usize) -> Self {
        Self {
            prefix,
            payload,
            suffix,
        }
    }

    /// Length of the entire frame (prefix + payload + suffix).
    #[inline]
    pub fn len(&self) -> usize {
        self.prefix + self.payload + self.suffix
    }

    /// Returns true if the frame has zero total length.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Prefix (header) length in bytes.
    #[inline]
    pub fn prefix(&self) -> usize {
        self.prefix
    }

    /// Payload length in bytes.
    #[inline]
    pub fn payload(&self) -> usize {
        self.payload
    }

    /// Suffix length in bytes.
    #[inline]
    pub fn suffix(&self) -> usize {
        self.suffix
    }

    /// Slice the payload out of an owned buffer as an owned [`Slice`] view.
    #[inline]
    pub fn slice<B: BoundedIoBuf>(&self, buf: B) -> Slice<B::Buf> {
        buf.slice(self.prefix..self.prefix + self.payload)
    }
}

/// Enclosing and extracting frames in an owned buffer.
pub trait Framer<B: IoBufMut> {
    /// Enclose a frame around the currently initialized bytes of `buf` in-place.
    ///
    /// All initialized bytes (`0..bytes_init`) are valid payload that must be
    /// enclosed. Implementations may reserve, shift data with
    /// [`IoBufMutExt::copy_within`], or append a delimiter.
    fn enclose(&mut self, buf: &mut B);

    /// Extract a frame from the given buffer view.
    ///
    /// Returns:
    /// - `Ok(Some(frame))` if a complete frame is found
    /// - `Ok(None)` if more data is needed
    /// - `Err` if the data is malformed
    fn extract(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>>;

    /// Extract a frame when the underlying stream has reached EOF.
    ///
    /// Default: same as [`Framer::extract`]. Delimiter framers override this to
    /// yield a final frame without a trailing delimiter. Length-delimited framers
    /// treat incomplete residual data as an error.
    fn extract_eof(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        self.extract(buf)
    }
}

/// Frames data with a big-endian (by default) length prefix.
///
/// Layout: `[length: N bytes][payload: length bytes]`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LengthDelimited {
    length_field_len: usize,
    length_field_is_big_endian: bool,
}

impl Default for LengthDelimited {
    fn default() -> Self {
        Self {
            length_field_len: 4,
            length_field_is_big_endian: true,
        }
    }
}

impl LengthDelimited {
    const MAX_LFL: usize = 8;

    /// Creates a new length-delimited framer with a 4-byte big-endian length field.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the length of the length field in bytes.
    #[inline]
    pub fn length_field_len(&self) -> usize {
        self.length_field_len
    }

    /// Sets the length of the length field in bytes (1..=8).
    ///
    /// # Panics
    ///
    /// Panics if `len_field_len` is greater than 8.
    pub fn set_length_field_len(mut self, len_field_len: usize) -> Self {
        assert!(
            len_field_len > 0 && len_field_len <= Self::MAX_LFL,
            "length field must be between 1 and 8 bytes"
        );
        self.length_field_len = len_field_len;
        self
    }

    /// Returns whether the length field is big-endian.
    #[inline]
    pub fn length_field_is_big_endian(&self) -> bool {
        self.length_field_is_big_endian
    }

    /// Sets whether the length field is big-endian.
    #[inline]
    pub fn set_length_field_is_big_endian(mut self, big_endian: bool) -> Self {
        self.length_field_is_big_endian = big_endian;
        self
    }
}

impl<B: IoBufMut + IoBufMutExt> Framer<B> for LengthDelimited {
    fn enclose(&mut self, buf: &mut B) {
        let len = buf.bytes_init();
        let lfl = self.length_field_len;

        buf.reserve(lfl);
        // Shift payload right to make room for the length prefix.
        buf.copy_within(0..len, lfl);
        debug_assert!(buf.bytes_init() >= lfl + len);

        let mut len_bytes = [0u8; Self::MAX_LFL];
        let len_u64 = len as u64;
        if self.length_field_is_big_endian {
            len_bytes[Self::MAX_LFL - lfl..].copy_from_slice(&len_u64.to_be_bytes()[Self::MAX_LFL - lfl..]);
            buf.as_mut_init()[..lfl].copy_from_slice(&len_bytes[Self::MAX_LFL - lfl..]);
        } else {
            len_bytes[..lfl].copy_from_slice(&len_u64.to_le_bytes()[..lfl]);
            buf.as_mut_init()[..lfl].copy_from_slice(&len_bytes[..lfl]);
        }
    }

    fn extract(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        // Use the initialized view (`Deref`), not `Slice::len()` which is the
        // full capacity window and may include uninitialized tail space.
        let data: &[u8] = buf;
        let lfl = self.length_field_len;
        if data.len() < lfl {
            return Ok(None);
        }

        let mut len_bytes = [0u8; Self::MAX_LFL];
        let payload_len = if self.length_field_is_big_endian {
            len_bytes[Self::MAX_LFL - lfl..].copy_from_slice(&data[..lfl]);
            u64::from_be_bytes(len_bytes) as usize
        } else {
            len_bytes[..lfl].copy_from_slice(&data[..lfl]);
            u64::from_le_bytes(len_bytes) as usize
        };

        if data.len() < lfl + payload_len {
            return Ok(None);
        }

        Ok(Some(Frame::new(lfl, payload_len, 0)))
    }

    fn extract_eof(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        match self.extract(buf)? {
            Some(frame) => Ok(Some(frame)),
            None => {
                let data: &[u8] = buf;
                if data.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "bytes remaining on stream",
                    ))
                }
            }
        }
    }
}

/// Delimiter that uses a single character encoded as UTF-8.\
///
/// If you need to use a multi-byte delimiter or other encodings, consider using [`AnyDelimited`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CharDelimited<const C: char> {
    char_buf: [u8; 4],
}

impl<const C: char> CharDelimited<C> {
    /// Creates a new character-delimited framer.
    #[inline]
    pub fn new() -> Self {
        Self { char_buf: [0; 4] }
    }

    fn delimiter_bytes(&mut self) -> &[u8] {
        C.encode_utf8(&mut self.char_buf).as_bytes()
    }
}

impl<B: IoBufMut + IoBufMutExt, const C: char> Framer<B> for CharDelimited<C> {
    fn enclose(&mut self, buf: &mut B) {
        let delim = {
            let bytes = C.encode_utf8(&mut self.char_buf).as_bytes();
            // Copy to stack so we can release the borrow on self.char_buf.
            let mut tmp = [0u8; 4];
            let n = bytes.len();
            tmp[..n].copy_from_slice(bytes);
            (tmp, n)
        };
        buf.extend_from_slice(&delim.0[..delim.1]);
    }

    fn extract(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        let data: &[u8] = buf;
        let delim = self.delimiter_bytes();
        let delim_len = delim.len();
        if data.is_empty() {
            return Ok(None);
        }
        if let Some(pos) = data.windows(delim_len).position(|w| w == delim) {
            Ok(Some(Frame::new(0, pos, delim_len)))
        } else {
            Ok(None)
        }
    }

    fn extract_eof(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        if let Some(frame) = self.extract(buf)? {
            return Ok(Some(frame));
        }
        let data: &[u8] = buf;
        if data.is_empty() {
            Ok(None)
        } else {
            // Final line without trailing delimiter.
            Ok(Some(Frame::new(0, data.len(), 0)))
        }
    }
}

/// Delimiter that uses an arbitrary byte sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnyDelimited<'a> {
    bytes: &'a [u8],
}

impl<'a> AnyDelimited<'a> {
    // Creates a new delimiter framer with the given delimiter bytes.
    #[inline]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl<B: IoBufMut + IoBufMutExt> Framer<B> for AnyDelimited<'_> {
    fn enclose(&mut self, buf: &mut B) {
        buf.extend_from_slice(self.bytes);
    }

    fn extract(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        let data: &[u8] = buf;
        if data.is_empty() || self.bytes.is_empty() {
            return Ok(None);
        }
        if let Some(pos) = data.windows(self.bytes.len()).position(|w| w == self.bytes) {
            Ok(Some(Frame::new(0, pos, self.bytes.len())))
        } else {
            Ok(None)
        }
    }

    fn extract_eof(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        if let Some(frame) = self.extract(buf)? {
            return Ok(Some(frame));
        }
        let data: &[u8] = buf;
        if data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Frame::new(0, data.len(), 0)))
        }
    }
}

/// Newline (`\n`) delimited framing.
pub type LineDelimited = CharDelimited<'\n'>;

/// A framer that does not add framing; yields chunks up to `max_size`.
///
/// It simply reserves space in the buffer without adding any framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoopFramer {
    max_size: usize,
}

impl Default for NoopFramer {
    fn default() -> Self {
        Self { max_size: 4096 }
    }
}

impl NoopFramer {
    // Creates a new no-op framer with a 4 KiB max chunk size.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    // Returns the maximum chunk size.
    #[inline]
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    // Sets the maximum chunk size.
    #[inline]
    pub fn set_max_size(mut self, max_size: usize) -> Self {
        self.max_size = max_size;
        self
    }
}

impl<B: IoBufMut> Framer<B> for NoopFramer {
    fn enclose(&mut self, _buf: &mut B) {}

    fn extract(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        let data: &[u8] = buf;
        if data.is_empty() {
            return Ok(None);
        }
        let len = data.len().min(self.max_size);
        Ok(Some(Frame::new(0, len, 0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::BoundedIoBuf;

    #[test]
    fn length_delimited_enclose_extract() {
        let mut framer = LengthDelimited::new();
        let mut buf = Vec::from(&b"hello"[..]);
        framer.enclose(&mut buf);
        assert_eq!(&buf[..9], b"\x00\x00\x00\x05hello");

        let view = buf.slice(..);
        let frame = framer.extract(&view).unwrap().unwrap();
        assert_eq!(frame, Frame::new(4, 5, 0));
        let payload = frame.slice(view.into_inner());
        assert_eq!(&payload[..], b"hello");
    }

    #[test]
    fn length_delimited_partial() {
        let mut framer = LengthDelimited::new();
        let buf = vec![0u8, 0, 0, 5, b'h', b'e'];
        let view = buf.slice(..);
        assert!(framer.extract(&view).unwrap().is_none());
    }

    #[test]
    fn test_char_delimited() {
        let mut framer = CharDelimited::<'ℝ'>::new();

        let mut buf = Vec::new();
        IoBufMutExt::extend_from_slice(&mut buf, b"hello");
        framer.enclose(&mut buf);
        assert_eq!(&buf[..], "helloℝ".as_bytes());

        let view = buf.slice(..);
        let frame = framer.extract(&view).unwrap().unwrap();
        assert_eq!(frame, Frame::new(0, 5, 3));
        let payload = frame.slice(view);
        assert_eq!(&payload[..], b"hello");
    }

    #[test]
    fn line_delimited() {
        let mut framer = LineDelimited::new();
        let mut buf = Vec::from(&b"hello"[..]);
        framer.enclose(&mut buf);
        assert_eq!(&buf[..], b"hello\n");

        let view = buf.slice(..);
        let frame = framer.extract(&view).unwrap().unwrap();
        assert_eq!(frame, Frame::new(0, 5, 1));
        let payload = frame.slice(view);
        assert_eq!(&payload[..], b"hello");
    }

    #[test]
    fn noop_framer() {
        let mut framer = NoopFramer::new().set_max_size(3);
        let buf = Vec::from(&b"hello"[..]);
        let view = buf.slice(..);
        let frame = framer.extract(&view).unwrap().unwrap();
        assert_eq!(frame, Frame::new(0, 3, 0));
    }

    #[test]
    fn line_delimited_extract_eof_without_newline() {
        let mut framer = LineDelimited::new();
        let buf = Vec::from(&b"hello"[..]);
        let view = buf.slice(..);
        assert!(framer.extract(&view).unwrap().is_none());
        let frame = framer.extract_eof(&view).unwrap().unwrap();
        assert_eq!(frame, Frame::new(0, 5, 0));
    }
}
