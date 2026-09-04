use std::io::{self, IoSlice, Write};

pub(crate) const CIPHERTEXT_CAPACITY: usize = 18 * 1024;
pub(crate) const PLAINTEXT_CAPACITY: usize = 16 * 1024;
pub(crate) const RUSTLS_BUFFER_LIMIT: usize = 64 * 1024;

/// A synchronous writer that can initialize only the spare capacity of a
/// reusable allocation. It never grows the allocation.
pub(crate) struct FixedWriter<'a> {
    buffer: &'a mut Vec<u8>,
    capacity: usize,
}

impl<'a> FixedWriter<'a> {
    pub(crate) fn new(buffer: &'a mut Vec<u8>, capacity: usize) -> Self {
        debug_assert!(buffer.len() <= capacity);
        debug_assert!(buffer.capacity() >= capacity);
        Self { buffer, capacity }
    }

    fn remaining(&self) -> usize {
        self.capacity.saturating_sub(self.buffer.len())
    }
}

impl Write for FixedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(self.remaining());
        self.buffer.extend_from_slice(&bytes[..count]);
        Ok(count)
    }

    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut written = 0;
        for buffer in buffers {
            if self.remaining() == 0 {
                break;
            }
            written += self.write(buffer)?;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{CIPHERTEXT_CAPACITY, FixedWriter};
    use std::io::{IoSlice, Write};

    #[test]
    fn fixed_writer_stops_at_capacity() {
        let mut output = Vec::with_capacity(CIPHERTEXT_CAPACITY);
        let pointer = output.as_ptr();
        let mut writer = FixedWriter::new(&mut output, CIPHERTEXT_CAPACITY);
        let buffers = [IoSlice::new(&[1; 16 * 1024]), IoSlice::new(&[2; 4 * 1024])];

        assert_eq!(writer.write_vectored(&buffers).unwrap(), CIPHERTEXT_CAPACITY);
        assert_eq!(output.len(), CIPHERTEXT_CAPACITY);
        assert_eq!(output.as_ptr(), pointer);
    }
}
