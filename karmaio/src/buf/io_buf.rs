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
