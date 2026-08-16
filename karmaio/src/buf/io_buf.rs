use std::{rc::Rc, sync::Arc};

/// An immutable buffer for completion-based I/O operations.
///
/// Operations take ownership of buffers and keep them in stable storage while
/// the operating system may access them. Implementors only expose a normal
/// borrowed slice and therefore do not need to uphold raw-pointer stability.
pub trait IoBuf: 'static {
    /// Returns the initialized bytes available to an output operation.
    fn as_init(&self) -> &[u8];
}

impl<B: IoBuf + ?Sized> IoBuf for &'static B {
    #[inline]
    fn as_init(&self) -> &[u8] {
        (**self).as_init()
    }
}

impl<B: IoBuf + ?Sized> IoBuf for &'static mut B {
    #[inline]
    fn as_init(&self) -> &[u8] {
        (**self).as_init()
    }
}

impl<B: IoBuf + ?Sized> IoBuf for Box<B> {
    #[inline]
    fn as_init(&self) -> &[u8] {
        (**self).as_init()
    }
}

impl<B: IoBuf + ?Sized> IoBuf for Rc<B> {
    #[inline]
    fn as_init(&self) -> &[u8] {
        (**self).as_init()
    }
}

impl<B: IoBuf + ?Sized> IoBuf for Arc<B> {
    #[inline]
    fn as_init(&self) -> &[u8] {
        (**self).as_init()
    }
}

impl IoBuf for [u8] {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

impl<const N: usize> IoBuf for [u8; N] {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

impl IoBuf for Vec<u8> {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

impl IoBuf for str {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl IoBuf for String {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[cfg(feature = "bytes")]
#[cfg_attr(docsrs, doc(cfg(feature = "bytes")))]
impl IoBuf for bytes::Bytes {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

#[cfg(feature = "bytes")]
#[cfg_attr(docsrs, doc(cfg(feature = "bytes")))]
impl IoBuf for bytes::BytesMut {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

#[cfg(feature = "memmap2")]
#[cfg_attr(docsrs, doc(cfg(feature = "memmap2")))]
impl IoBuf for memmap2::Mmap {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

#[cfg(feature = "memmap2")]
#[cfg_attr(docsrs, doc(cfg(feature = "memmap2")))]
impl IoBuf for memmap2::MmapMut {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

#[cfg(all(test, feature = "bytes"))]
mod bytes_tests {
    use super::IoBuf;

    #[test]
    fn bytes_as_init_returns_initialized_bytes() {
        let buf = bytes::Bytes::from_static(b"hello");
        assert_eq!(buf.as_init(), b"hello");
    }

    #[test]
    fn bytes_mut_as_init_returns_initialized_prefix() {
        let mut buf = bytes::BytesMut::with_capacity(16);
        buf.extend_from_slice(b"abc");
        assert_eq!(buf.as_init(), b"abc");
    }

    #[test]
    fn bytes_can_be_copied_into_a_write_destination() {
        fn append<B: IoBuf>(dst: &mut Vec<u8>, buf: B) {
            dst.extend_from_slice(buf.as_init());
        }

        let mut dst = Vec::new();
        append(&mut dst, bytes::Bytes::from_static(b"hi"));
        assert_eq!(dst, b"hi");
    }
}

#[cfg(all(test, feature = "memmap2"))]
mod memmap2_tests {
    use super::IoBuf;

    #[test]
    fn mmap_as_init_returns_mapped_bytes() {
        let mut mmap_mut = memmap2::MmapMut::map_anon(5).unwrap();
        mmap_mut[..].copy_from_slice(b"hello");
        let mmap = mmap_mut.make_read_only().unwrap();
        assert_eq!(mmap.as_init(), b"hello");
    }

    #[test]
    fn mmap_mut_as_init_returns_all_mapped_bytes() {
        let mut buf = memmap2::MmapMut::map_anon(5).unwrap();
        buf[..].copy_from_slice(b"abcde");
        assert_eq!(buf.as_init(), b"abcde");
    }

    #[test]
    fn mmap_can_be_copied_into_a_write_destination() {
        fn append<B: IoBuf>(dst: &mut Vec<u8>, buf: B) {
            dst.extend_from_slice(buf.as_init());
        }

        let mut mmap_mut = memmap2::MmapMut::map_anon(2).unwrap();
        mmap_mut[..].copy_from_slice(b"hi");
        let mmap = mmap_mut.make_read_only().unwrap();

        let mut dst = Vec::new();
        append(&mut dst, mmap);
        assert_eq!(dst, b"hi");
    }
}
