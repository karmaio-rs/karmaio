use std::io;

use crate::{
    buf::{IoBuf, IoBufExt, IoBufMut, IoBufMutExt, Slice},
    io::framed::buffer::append,
};

const DEFAULT_MAX_FRAME_LENGTH: usize = 8 * 1024 * 1024;

/// An extracted frame describing where the payload sits inside a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// Offset where the frame payload starts.
    prefix: usize,
    /// Length of the frame payload.
    payload: usize,
    /// Suffix length after the payload, such as a delimiter.
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

    pub(super) fn checked_bounds(&self, available: usize) -> io::Result<(std::ops::Range<usize>, usize)> {
        let payload_end = self.prefix.checked_add(self.payload).ok_or_else(invalid_frame)?;
        let frame_end = payload_end.checked_add(self.suffix).ok_or_else(invalid_frame)?;
        if frame_end == 0 || frame_end > available {
            return Err(invalid_frame());
        }
        Ok((self.prefix..payload_end, frame_end))
    }

    /// Slice the payload out of an owned buffer as an owned [`Slice`] view.
    #[inline]
    pub fn slice<B: IoBuf>(&self, buf: B) -> Slice<B> {
        buf.slice(self.prefix..self.prefix + self.payload)
    }
}

fn invalid_frame() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "framer returned invalid frame bounds")
}

/// Enclosing and extracting frames in an owned buffer.
pub trait Framer<B: IoBufMut> {
    /// Enclose a frame around the currently initialized bytes of `buf` in-place.
    ///
    /// All initialized bytes (`0..bytes_init`) are valid payload that must be
    /// enclosed. Implementations may reserve, shift data with
    /// [`IoBufMutExt::copy_within`], or append a delimiter.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload cannot be represented or the buffer
    /// cannot grow to hold the enclosing bytes.
    fn enclose(&mut self, buf: &mut B) -> io::Result<()>;

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
    max_frame_length: usize,
}

impl Default for LengthDelimited {
    fn default() -> Self {
        Self {
            length_field_len: 4,
            length_field_is_big_endian: true,
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
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
    /// Panics unless `len_field_len` is between 1 and 8, inclusive.
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

    /// Returns the largest payload accepted from or written to the wire.
    #[inline]
    pub fn max_frame_length(&self) -> usize {
        self.max_frame_length
    }

    /// Sets the largest payload accepted from or written to the wire.
    #[inline]
    pub fn set_max_frame_length(mut self, max_frame_length: usize) -> Self {
        self.max_frame_length = max_frame_length;
        self
    }
}

impl<B: IoBufMut + IoBufMutExt> Framer<B> for LengthDelimited {
    fn enclose(&mut self, buf: &mut B) -> io::Result<()> {
        let len = IoBuf::as_init(buf).len();
        let lfl = self.length_field_len;

        let len_u64 = u64::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame payload length exceeds u64"))?;
        if len > self.max_frame_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame payload length exceeds configured maximum",
            ));
        }
        let max_payload = if lfl == Self::MAX_LFL {
            u64::MAX
        } else {
            (1_u64 << (lfl * 8)) - 1
        };
        if len_u64 > max_payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame payload length does not fit the configured length field",
            ));
        }
        let enclosed_len = (lfl as u64)
            .checked_add(len_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "enclosed frame length overflow"))?;

        buf.reserve(lfl)?;
        if buf.as_uninit().len() < enclosed_len {
            return Err(io::Error::other(
                "buffer reserve did not provide space for the framed payload",
            ));
        }
        // Shift payload right to make room for the length prefix.
        buf.copy_within(0..len, lfl);

        let len_bytes = if self.length_field_is_big_endian {
            len_u64.to_be_bytes()
        } else {
            len_u64.to_le_bytes()
        };
        let prefix = if self.length_field_is_big_endian {
            &len_bytes[Self::MAX_LFL - lfl..]
        } else {
            &len_bytes[..lfl]
        };

        // Safety: the prefix destination is in the uniquely borrowed buffer,
        // `copy_within` initialized the shifted payload, and `set_len` is only
        // called after every byte in the enlarged initialized range was written.
        unsafe {
            std::ptr::copy_nonoverlapping(prefix.as_ptr(), buf.as_uninit().as_mut_ptr().cast::<u8>(), lfl);
            buf.set_len(enclosed_len);
        }
        Ok(())
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
        let encoded_len = if self.length_field_is_big_endian {
            len_bytes[Self::MAX_LFL - lfl..].copy_from_slice(&data[..lfl]);
            u64::from_be_bytes(len_bytes)
        } else {
            len_bytes[..lfl].copy_from_slice(&data[..lfl]);
            u64::from_le_bytes(len_bytes)
        };
        let payload_len = usize::try_from(encoded_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "decoded payload length exceeds usize"))?;
        if payload_len > self.max_frame_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decoded frame length exceeds configured maximum",
            ));
        }

        let frame_len = lfl
            .checked_add(payload_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "decoded frame length overflow"))?;
        if data.len() < frame_len {
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

/// Delimiter that uses a single character encoded as UTF-8.
///
/// If you need to use a multi-byte delimiter or other encodings, consider using [`AnyDelimited`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CharDelimited<const C: char> {
    char_buf: [u8; 4],
    next_index: usize,
    max_length: usize,
}

impl<const C: char> Default for CharDelimited<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const C: char> CharDelimited<C> {
    /// Creates a new character-delimited framer.
    #[inline]
    pub fn new() -> Self {
        Self {
            char_buf: [0; 4],
            next_index: 0,
            max_length: DEFAULT_MAX_FRAME_LENGTH,
        }
    }

    /// Creates a delimiter framer with a maximum inbound payload length.
    #[inline]
    pub fn new_with_max_length(max_length: usize) -> Self {
        Self {
            max_length,
            ..Self::new()
        }
    }

    /// Returns the maximum inbound payload length.
    #[inline]
    pub fn max_length(&self) -> usize {
        self.max_length
    }

    /// Sets the maximum inbound payload length.
    #[inline]
    pub fn set_max_length(mut self, max_length: usize) -> Self {
        self.max_length = max_length;
        self
    }

    fn delimiter_bytes(&mut self) -> &[u8] {
        C.encode_utf8(&mut self.char_buf).as_bytes()
    }
}

impl<B: IoBufMut + IoBufMutExt, const C: char> Framer<B> for CharDelimited<C> {
    fn enclose(&mut self, buf: &mut B) -> io::Result<()> {
        let delim = {
            let bytes = C.encode_utf8(&mut self.char_buf).as_bytes();
            // Copy to stack so we can release the borrow on self.char_buf.
            let mut tmp = [0u8; 4];
            let n = bytes.len();
            tmp[..n].copy_from_slice(bytes);
            (tmp, n)
        };
        append(buf, &delim.0[..delim.1])
    }

    fn extract(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        let data: &[u8] = buf;
        let (delimiter, delim_len) = {
            let delim = self.delimiter_bytes();
            let mut delimiter = [0; 4];
            delimiter[..delim.len()].copy_from_slice(delim);
            (delimiter, delim.len())
        };
        let delim = &delimiter[..delim_len];
        extract_delimited(data, delim, &mut self.next_index, self.max_length)
    }

    fn extract_eof(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        if let Some(frame) = self.extract(buf)? {
            return Ok(Some(frame));
        }
        let data: &[u8] = buf;
        if data.is_empty() {
            self.next_index = 0;
            Ok(None)
        } else if data.len() > self.max_length {
            self.next_index = 0;
            Err(delimited_frame_too_large())
        } else {
            // Final line without trailing delimiter.
            self.next_index = 0;
            Ok(Some(Frame::new(0, data.len(), 0)))
        }
    }
}

/// Delimiter that uses an arbitrary byte sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnyDelimited<'a> {
    bytes: &'a [u8],
    next_index: usize,
    max_length: usize,
}

impl<'a> AnyDelimited<'a> {
    /// Creates a new delimiter framer with the given delimiter bytes.
    ///
    /// # Panics
    ///
    /// Panics if `bytes` is empty because an empty byte sequence cannot make
    /// framing progress.
    #[inline]
    pub fn new(bytes: &'a [u8]) -> Self {
        assert!(!bytes.is_empty(), "delimiter must not be empty");
        Self {
            bytes,
            next_index: 0,
            max_length: DEFAULT_MAX_FRAME_LENGTH,
        }
    }

    /// Creates a delimiter framer with a maximum inbound payload length.
    ///
    /// # Panics
    ///
    /// Panics if `bytes` is empty.
    #[inline]
    pub fn new_with_max_length(bytes: &'a [u8], max_length: usize) -> Self {
        Self {
            max_length,
            ..Self::new(bytes)
        }
    }

    /// Returns the maximum inbound payload length.
    #[inline]
    pub fn max_length(&self) -> usize {
        self.max_length
    }

    /// Sets the maximum inbound payload length.
    #[inline]
    pub fn set_max_length(mut self, max_length: usize) -> Self {
        self.max_length = max_length;
        self
    }
}

impl<B: IoBufMut + IoBufMutExt> Framer<B> for AnyDelimited<'_> {
    fn enclose(&mut self, buf: &mut B) -> io::Result<()> {
        append(buf, self.bytes)
    }

    fn extract(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        let data: &[u8] = buf;
        extract_delimited(data, self.bytes, &mut self.next_index, self.max_length)
    }

    fn extract_eof(&mut self, buf: &Slice<B>) -> io::Result<Option<Frame>> {
        if let Some(frame) = self.extract(buf)? {
            return Ok(Some(frame));
        }
        let data: &[u8] = buf;
        if data.is_empty() {
            self.next_index = 0;
            Ok(None)
        } else if data.len() > self.max_length {
            self.next_index = 0;
            Err(delimited_frame_too_large())
        } else {
            self.next_index = 0;
            Ok(Some(Frame::new(0, data.len(), 0)))
        }
    }
}

fn extract_delimited(
    data: &[u8],
    delimiter: &[u8],
    next_index: &mut usize,
    max_length: usize,
) -> io::Result<Option<Frame>> {
    if data.is_empty() {
        return Ok(None);
    }

    let delimiter_len = delimiter.len();
    let read_to = data.len().min(max_length.saturating_add(delimiter_len));
    let search_from = (*next_index).min(read_to);
    if let Some(offset) = data[search_from..read_to]
        .windows(delimiter_len)
        .position(|window| window == delimiter)
    {
        let position = search_from + offset;
        *next_index = 0;
        return Ok(Some(Frame::new(0, position, delimiter_len)));
    }

    if data.len() > max_length {
        let possible_delimiter_start = (1..delimiter_len)
            .rev()
            .find(|&overlap| data.ends_with(&delimiter[..overlap]))
            .map(|overlap| data.len() - overlap);
        if possible_delimiter_start.is_none_or(|start| start > max_length) {
            *next_index = 0;
            return Err(delimited_frame_too_large());
        }
    }

    *next_index = read_to.saturating_sub(delimiter_len.saturating_sub(1));
    Ok(None)
}

fn delimited_frame_too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "delimited frame exceeds configured maximum")
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
    /// Creates a new no-op framer with a 4 KiB maximum chunk size.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the maximum chunk size.
    #[inline]
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Sets the maximum chunk size.
    #[inline]
    pub fn set_max_size(mut self, max_size: usize) -> Self {
        assert!(max_size > 0, "no-op frame size must be non-zero");
        self.max_size = max_size;
        self
    }
}

impl<B: IoBufMut> Framer<B> for NoopFramer {
    fn enclose(&mut self, _buf: &mut B) -> io::Result<()> {
        Ok(())
    }

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

    #[test]
    fn length_delimited_enclose_extract() {
        let mut framer = LengthDelimited::new();
        let mut buf = Vec::from(&b"hello"[..]);
        framer.enclose(&mut buf).unwrap();
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
    fn length_delimited_rejects_payload_that_does_not_fit_field() {
        let mut framer = LengthDelimited::new().set_length_field_len(1);
        let mut buf = vec![0; 256];
        let error = framer.enclose(&mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(buf.len(), 256);
    }

    #[test]
    fn length_delimited_enforces_configured_maximum() {
        let mut framer = LengthDelimited::new().set_max_frame_length(3);
        let mut payload = b"four".to_vec();
        assert_eq!(
            framer.enclose(&mut payload).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let view = b"\0\0\0\x04four".to_vec().slice(..);
        assert_eq!(framer.extract(&view).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn delimiter_scan_resumes_near_the_previous_end() {
        let mut framer = AnyDelimited::new(b"END");
        let first = b"abcdefghijEN".to_vec().slice(..);
        assert!(framer.extract(&first).unwrap().is_none());
        assert_eq!(framer.next_index, 10);

        let second = b"abcdefghijEND".to_vec().slice(..);
        assert_eq!(framer.extract(&second).unwrap(), Some(Frame::new(0, 10, 3)));
        assert_eq!(framer.next_index, 0);
    }

    #[test]
    fn delimiter_enforces_configured_maximum() {
        let mut framer = AnyDelimited::new_with_max_length(b"\n", 3);
        let view = b"four".to_vec().slice(..);
        assert_eq!(framer.extract(&view).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn byte_delimiter_allows_a_fragment_after_the_maximum_payload() {
        let mut framer = AnyDelimited::new_with_max_length(b"END", 3);
        let partial = b"abcE".to_vec().slice(..);
        assert!(framer.extract(&partial).unwrap().is_none());

        let complete = b"abcEND".to_vec().slice(..);
        assert_eq!(framer.extract(&complete).unwrap(), Some(Frame::new(0, 3, 3)));
    }

    #[test]
    fn character_delimiter_allows_a_fragment_after_the_maximum_payload() {
        let mut framer = CharDelimited::<'ℝ'>::new_with_max_length(3);
        let delimiter = "ℝ".as_bytes();

        let mut partial = b"abc".to_vec();
        partial.extend_from_slice(&delimiter[..2]);
        assert!(framer.extract(&partial.slice(..)).unwrap().is_none());

        let mut complete = b"abc".to_vec();
        complete.extend_from_slice(delimiter);
        assert_eq!(framer.extract(&complete.slice(..)).unwrap(), Some(Frame::new(0, 3, 3)));
    }

    #[test]
    fn fragmented_delimiter_does_not_hide_an_oversized_payload() {
        let mut framer = AnyDelimited::new_with_max_length(b"END", 3);
        let view = b"abcdE".to_vec().slice(..);
        assert_eq!(framer.extract(&view).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn eof_rejects_a_partial_delimiter_beyond_the_maximum_payload() {
        let mut framer = AnyDelimited::new_with_max_length(b"END", 3);
        let view = b"abcE".to_vec().slice(..);
        assert_eq!(
            framer.extract_eof(&view).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    #[should_panic(expected = "delimiter must not be empty")]
    fn delimiter_rejects_an_empty_sequence() {
        let _ = AnyDelimited::new(b"");
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn length_delimited_rejects_decoded_length_overflow() {
        let mut framer = LengthDelimited::new().set_length_field_len(8);
        let view = vec![u8::MAX; 8].slice(..);
        let error = framer.extract(&view).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_bounds_reject_overflow_and_non_progress() {
        assert_eq!(
            Frame::new(usize::MAX, 1, 0).checked_bounds(8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            Frame::new(0, 0, 0).checked_bounds(8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn test_char_delimited() {
        let mut framer = CharDelimited::<'ℝ'>::new();

        let mut buf = Vec::new();
        IoBufMutExt::extend_from_slice(&mut buf, b"hello");
        framer.enclose(&mut buf).unwrap();
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
        framer.enclose(&mut buf).unwrap();
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
    #[should_panic(expected = "no-op frame size must be non-zero")]
    fn noop_framer_rejects_zero_maximum() {
        let _ = NoopFramer::new().set_max_size(0);
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
