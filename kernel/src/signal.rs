//! Per-process signal bookkeeping.
//!
//! Signals are still delivered at safe points: after a syscall or when a PIT
//! tick interrupts ring 3. This level adds the rest of the basic Unix model:
//! explicit ignore/default dispositions, blocked signals with pending bits,
//! default termination for fatal signals, and pending signals queued by a
//! handler being delivered as soon as that handler calls `sigreturn`.

/// Highest signal number accepted by the kernel.
pub const MAX_SIGNAL: u8 = 31;

/// Interrupt from the terminal; terminates by default.
pub const SIGINT: u8 = 2;
/// Illegal instruction; terminates by default.
pub const SIGILL: u8 = 4;
/// Bus error / bad memory access; terminates by default.
pub const SIGBUS: u8 = 7;
/// Arithmetic exception; terminates by default.
pub const SIGFPE: u8 = 8;
/// Kill; cannot be caught, ignored or blocked.
pub const SIGKILL: u8 = 9;
/// User-defined signal 1; ignored by default.
pub const SIGUSR1: u8 = 10;
/// Segmentation fault / invalid memory access; terminates by default.
pub const SIGSEGV: u8 = 11;
/// User-defined signal 2; ignored by default.
pub const SIGUSR2: u8 = 12;
/// Termination request; terminates by default.
pub const SIGTERM: u8 = 15;

/// Reset a signal to its default disposition.
pub const SIG_DFL: u64 = 0;
/// Ignore a signal explicitly.
pub const SIG_IGN: u64 = 1;

/// What the kernel should do when a pending signal is selected for delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    /// Discard the signal.
    Ignore,
    /// Terminate the process with a negative signal status.
    Terminate,
    /// Enter this user-space handler.
    Handler(u64),
}

/// State owned by one process. It is plain data because the process table lock
/// already serialises every access.
#[derive(Clone, Debug)]
pub struct SignalState {
    pending: u64,
    handlers: [u64; (MAX_SIGNAL + 1) as usize],
    mask: u32,
    in_handler: bool,
}

impl SignalState {
    /// Creates state with no pending signals and default dispositions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: 0,
            handlers: [SIG_DFL; (MAX_SIGNAL + 1) as usize],
            mask: 0,
            in_handler: false,
        }
    }

    /// Whether `signum` is a valid signal number.
    #[must_use]
    pub const fn valid(signum: u8) -> bool {
        signum > 0 && signum <= MAX_SIGNAL
    }

    /// The disposition for a signal right now, including its default action.
    #[must_use]
    pub fn disposition(&self, signum: u8) -> Disposition {
        if !Self::valid(signum) {
            return Disposition::Ignore;
        }
        // `SIGKILL` deliberately ignores both handlers and the mask.
        if signum == SIGKILL {
            return Disposition::Terminate;
        }
        match self.handlers[usize::from(signum)] {
            SIG_DFL => Self::default_disposition(signum),
            SIG_IGN => Disposition::Ignore,
            handler => Disposition::Handler(handler),
        }
    }

    /// The default action of a signal without a registered handler.
    const fn default_disposition(signum: u8) -> Disposition {
        match signum {
            SIGINT | SIGILL | SIGBUS | SIGFPE | SIGKILL | SIGSEGV | SIGTERM => {
                Disposition::Terminate
            }
            _ => Disposition::Ignore,
        }
    }

    /// Returns the registered handler address, or 0 when none is installed.
    #[must_use]
    pub fn handler(&self, signum: u8) -> u64 {
        self.handlers[usize::from(signum)]
    }

    /// Installs a disposition and returns the previous one.
    ///
    /// `SIG_DFL` resets to the default action and `SIG_IGN` explicitly ignores
    /// the signal. Installing `SIG_IGN` also drops a signal that was waiting for
    /// the old disposition.
    pub fn set_handler(&mut self, signum: u8, handler: u64) -> u64 {
        let slot = &mut self.handlers[usize::from(signum)];
        let previous = core::mem::replace(slot, handler);
        if handler == SIG_IGN {
            self.pending &= !u64::from(Self::bit(signum));
        }
        previous
    }

    /// Whether this signal is currently blocked. `SIGKILL` never is.
    #[must_use]
    pub const fn is_blocked(&self, signum: u8) -> bool {
        if signum == SIGKILL || !Self::valid(signum) {
            return false;
        }
        self.mask & Self::bit(signum) != 0
    }

    /// The current signal mask as one bit per signal (`signum - 1`).
    #[must_use]
    pub const fn mask(&self) -> u32 {
        self.mask
    }

    /// Replaces the mask and returns the previous one. `SIGKILL` is ignored.
    pub fn set_mask(&mut self, mask: u32) -> u32 {
        let previous = self.mask;
        self.mask = mask & !Self::bit(SIGKILL);
        previous
    }

    /// Adds signals to the mask and returns the previous mask.
    pub fn block(&mut self, mask: u32) -> u32 {
        let previous = self.mask;
        self.mask = (previous | mask) & !Self::bit(SIGKILL);
        previous
    }

    /// Removes signals from the mask and returns the previous mask.
    pub fn unblock(&mut self, mask: u32) -> u32 {
        let previous = self.mask;
        self.mask = previous & !mask;
        previous
    }

    /// The pending signals as one bit per signal (`signum - 1`).
    #[must_use]
    pub const fn pending_bits(&self) -> u32 {
        self.pending as u32
    }

    /// Queues a signal that has a non-ignored disposition.
    pub fn queue(&mut self, signum: u8) -> bool {
        if !Self::valid(signum) || self.disposition(signum) == Disposition::Ignore {
            return false;
        }
        self.pending |= u64::from(Self::bit(signum));
        true
    }

    /// Drops all queued signals, used when a child is forked.
    pub fn clear_pending(&mut self) {
        self.pending = 0;
        self.in_handler = false;
    }

    /// Takes the lowest-numbered deliverable pending signal.
    pub fn take_pending(&mut self) -> Option<(u8, Disposition)> {
        for signum in 1..=MAX_SIGNAL {
            let bit = u64::from(Self::bit(signum));
            if self.pending & bit != 0 && !self.is_blocked(signum) {
                self.pending &= !bit;
                return Some((signum, self.disposition(signum)));
            }
        }
        None
    }

    /// Whether a handler is currently running in this process.
    #[must_use]
    pub const fn in_handler(&self) -> bool {
        self.in_handler
    }

    /// Records that a handler has been entered or has returned.
    pub fn set_in_handler(&mut self, value: bool) {
        self.in_handler = value;
    }

    /// The mask bit for a signal number.
    const fn bit(signum: u8) -> u32 {
        1_u32 << (signum - 1)
    }
}

impl Default for SignalState {
    fn default() -> Self {
        Self::new()
    }
}
