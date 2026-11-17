//! The async keyboard: the interrupt handler queues raw scancodes, a task decodes
//! them into the kernel's input queue.
//!
//! Post-07's keyboard handler decoded a scancode *inside* the interrupt and pushed
//! the result straight into [`crate::input`]. Post-12 splits the two: the handler
//! only pushes the raw scancode into a lock-free queue and wakes the task that
//! owns the decoding. That keeps the interrupt handler short and makes the
//! decoder an ordinary `async` function that suspends until the next key arrives.
//!
//! The queue lives in a [`OnceCell`] because the handler must be able to reach it
//! as soon as the first key is pressed, while the task that reads it may not have
//! been polled yet. The [`AtomicWaker`] is what turns "a scancode arrived" into
//! "poll the task again".
//!
//! The decoded bytes go to [`crate::input`] exactly as before, so the shell and
//! the desktop keep reading them with `input::pop`; they only have to poll the
//! runtime ([`crate::task::poll`]) first.

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

/// Creates the shared scancode queue.
///
/// [`crate::task::init`] calls this before the keyboard IRQ is unmasked, so
/// [`add_scancode`] always finds an initialised queue. It is idempotent: the
/// queue is a single shared resource, and a second call leaves it in place.
pub fn init() {
    let _ = SCANCODE_QUEUE.try_init_once(|| ArrayQueue::new(QUEUE_CAPACITY));
}

/// Called by the keyboard interrupt handler with one raw scancode.
///
/// Must not block or allocate: it only pushes into the lock-free queue and wakes
/// the decoding task.
pub fn add_scancode(scancode: u8) {
    match SCANCODE_QUEUE.try_get() {
        Ok(queue) => match queue.push(scancode) {
            Ok(()) => WAKER.wake(),
            Err(_) => {
                crate::console::println!("WARNING: scancode queue full; dropping keyboard input");
            }
        },
        Err(_) => crate::console::println!("WARNING: scancode queue uninitialized"),
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
    /// It is safe to call more than once: the queue is created once and shared,
    /// and [`init`] has usually created it already.
    #[must_use]
    pub fn new() -> Self {
        init();
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

/// The async task that decodes scancodes into the kernel's input queue.
///
/// The decoder is the hand-written one from [`crate::keyboard`]; it pushes the
/// bytes it produces into [`crate::input`], which the serial interrupt also
/// writes to. Its push runs with interrupts disabled so the two producers never
/// interleave — the same discipline the interrupt handlers follow.
pub async fn print_keypresses() {
    let mut scancodes = ScancodeStream::new();
    while let Some(scancode) = scancodes.next().await {
        x86_64::instructions::interrupts::without_interrupts(|| {
            keyboard::handle_scancode(scancode);
        });
    }
}
