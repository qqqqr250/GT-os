//! The async keyboard: the interrupt handler pushes raw scancodes, a task decodes
//! them.
//!
//! Post-07's keyboard handler decodes a scancode *inside* the interrupt and drops
//! the result into a queue. Post-12 splits the two: the handler only pushes the
//! raw scancode into a lock-free queue and wakes the task that owns the decoding.
//! That keeps the interrupt handler short and makes the decoder an ordinary
//! `async` function that suspends until the next key arrives.
//!
//! The queue lives in a [`OnceCell`] because the handler must be able to reach it
//! as soon as the first key is pressed, while the task that creates it may not
//! have been polled yet. The [`AtomicWaker`] is what turns "a scancode arrived"
//! into "poll the task again".

use core::pin::Pin;
use core::task::{Context, Poll};

use futures_util::stream::{Stream, StreamExt};
use futures_util::task::AtomicWaker;

use crate::keyboard;
use crate::sync::{ArrayQueue, OnceCell};

/// How many raw scancodes the queue holds before it starts dropping input.
///
/// A human cannot type faster than the task drains, so this is generous; a full
/// queue means something else is wrong and the drop is reported.
const QUEUE_CAPACITY: usize = 100;

/// Raw scancodes the interrupt handler has pushed and the task has not read.
static SCANCODE_QUEUE: OnceCell<ArrayQueue<u8>> = OnceCell::uninit();

/// The waker of the task that is waiting for the next scancode.
static WAKER: AtomicWaker = AtomicWaker::new();

/// Called by the keyboard interrupt handler with one raw scancode.
///
/// Must not block or allocate: it only pushes into the lock-free queue and wakes
/// the decoding task.
pub fn add_scancode(scancode: u8) {
    match SCANCODE_QUEUE.try_get() {
        Ok(queue) => match queue.push(scancode) {
            Ok(()) => WAKER.wake(),
            Err(_) => crate::println!("WARNING: scancode queue full; dropping keyboard input"),
        },
        Err(_) => crate::println!("WARNING: scancode queue uninitialized"),
    }
}

/// The stream of raw scancodes the keyboard produces.
pub struct ScancodeStream {
    /// Keeps the struct unconstructible except through [`ScancodeStream::new`].
    _private: (),
}

impl ScancodeStream {
    /// Creates the stream, initialising the shared queue on first use.
    ///
    /// # Panics
    ///
    /// Panics if it is called more than once: the queue is a single shared
    /// resource, and a second stream would race the first for its scancodes.
    #[must_use]
    pub fn new() -> Self {
        SCANCODE_QUEUE
            .try_init_once(|| ArrayQueue::new(QUEUE_CAPACITY))
            .expect("ScancodeStream::new must only be called once");
        Self { _private: () }
    }
}

impl Default for ScancodeStream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream for ScancodeStream {
    type Item = u8;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<u8>> {
        let queue = SCANCODE_QUEUE
            .try_get()
            .expect("scancode queue not initialized");

        // Fast path: a scancode is already waiting, so there is no need to touch
        // the waker at all.
        if let Ok(scancode) = queue.pop() {
            return Poll::Ready(Some(scancode));
        }

        // Register first, then re-check: a scancode pushed between the first pop
        // and the registration would otherwise be missed until the *next* key.
        WAKER.register(cx.waker());
        match queue.pop() {
            Ok(scancode) => {
                WAKER.take();
                Poll::Ready(Some(scancode))
            }
            Err(_) => Poll::Pending,
        }
    }
}

/// The async task that decodes scancodes and echoes what was typed.
///
/// The decoder itself is the hand-written one from [`crate::keyboard`]; the task
/// only feeds it and prints the bytes it produces.
pub async fn print_keypresses() {
    let mut scancodes = ScancodeStream::new();
    while let Some(scancode) = scancodes.next().await {
        keyboard::handle_scancode(scancode);
        while let Some(byte) = keyboard::pop() {
            print_byte(byte);
        }
    }
}

/// Prints one decoded byte, spelling out the non-ASCII control codes.
fn print_byte(byte: u8) {
    match byte {
        keyboard::KEY_UP => crate::print!("<up>"),
        keyboard::KEY_DOWN => crate::print!("<down>"),
        keyboard::KEY_LEFT => crate::print!("<left>"),
        keyboard::KEY_RIGHT => crate::print!("<right>"),
        0x08 => crate::print!("<bs>"),
        0x1b => crate::print!("<esc>"),
        _ => crate::print!("{}", char::from(byte)),
    }
}
