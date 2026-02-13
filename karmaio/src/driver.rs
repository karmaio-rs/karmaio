use std::{
    io,
    task::{Context, Poll},
    time::Duration,
};

use crate::driver::ops::Op;

pub(crate) mod drivers;
pub(crate) mod ops;

pub(crate) trait Driver {
    // Wait infinitely and process returned events.
    fn wait(&self) -> io::Result<usize>;
    // Wait for specified timeout and process returned events.
    fn wait_with_duration(&self, duration: Duration) -> io::Result<usize>;
    // Submit an op to the driver
    fn submit_op<T>(&mut self, data: T) -> io::Result<Op<T>>;
    // Remove an op from the driver
    fn remove_op<T>(&mut self, data: &mut T);
    // Poll an op using the driver
    fn poll_op<T: Future>(&mut self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Output>;
}
