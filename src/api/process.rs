use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    array,
    ops::{Index, IndexMut},
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use bitflags::bitflags;
use kspin::SpinNoIrq;
use strum::IntoEnumIterator;

use crate::{
    DefaultSignalAction, PendingSignals, SignalAction, SignalActionFlags, SignalDisposition,
    SignalInfo, SignalSet, Signo, api::ThreadSignalManager,
};

/// Signal actions for a process.
#[derive(Clone)]
pub struct SignalActions(pub(crate) [SignalAction; 64]);

impl Default for SignalActions {
    fn default() -> Self {
        Self(array::from_fn(|_| SignalAction::default()))
    }
}

impl Index<Signo> for SignalActions {
    type Output = SignalAction;

    fn index(&self, signo: Signo) -> &SignalAction {
        &self.0[signo as usize - 1]
    }
}

impl IndexMut<Signo> for SignalActions {
    fn index_mut(&mut self, signo: Signo) -> &mut SignalAction {
        &mut self.0[signo as usize - 1]
    }
}

bitflags! {
    /// A bitflag representing signal-stop and signal-continue event
    pub struct SignalEventFlags: u8 {
        const PENDING_STOP_EVENT = 1 << 0;
        const PENDING_CONT_EVENT = 1 << 1;
    }
}

/// Process-level signal manager.
pub struct ProcessSignalManager {
    /// The process-level shared pending signals
    pending: SpinNoIrq<PendingSignals>,

    /// The signal actions
    pub actions: Arc<SpinNoIrq<SignalActions>>,

    /// The default restorer function.
    pub(crate) default_restorer: usize,

    /// Thread-level signal managers.
    pub(crate) children: SpinNoIrq<Vec<(u32, Weak<ThreadSignalManager>)>>,

    pub(crate) possibly_has_signal: AtomicBool,

    /// Signal event flag, keep track of un-consumed stop/continue event by
    /// `wait`
    signal_events: AtomicU8,
    /// The signal stops the process most recently
    last_stop_signal: SpinNoIrq<Option<Signo>>,
}

impl ProcessSignalManager {
    /// Creates a new process signal manager.
    pub fn new(actions: Arc<SpinNoIrq<SignalActions>>, default_restorer: usize) -> Self {
        Self {
            pending: SpinNoIrq::new(PendingSignals::default()),
            actions,
            default_restorer,
            children: SpinNoIrq::new(Vec::new()),
            possibly_has_signal: AtomicBool::new(false),
            signal_events: AtomicU8::new(0),
            last_stop_signal: SpinNoIrq::new(None),
        }
    }

    pub(crate) fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        let mut guard = self.pending.lock();
        let result = guard.dequeue_signal(mask);
        if guard.set.is_empty() {
            self.possibly_has_signal.store(false, Ordering::Release);
        }
        result
    }

    /// Checks if a signal is ignored by the process.
    /// Only discard signals that have no side effects AND are ignored.
    /// SIGCONT and SIGKILL shall always be queued for their side effects.
    pub fn signal_ignored(&self, signo: Signo) -> bool {
        // A speical case for SIGCONT (and may also inlcude SIGKILL even it cannot be
        // ignored).
        // Per POSIX.1-2024, when a process is stopped, SIGCONT should
        // be able to continue this process even if the process may ignore the
        // SIGCONT signal. We use the `has_side_effect` function here to deliver
        // the SIGCONT under all circumstance in an early return.
        if signo.has_side_effect() {
            return false;
        }
        match &self.actions.lock()[signo].disposition {
            SignalDisposition::Ignore => true,
            SignalDisposition::Default => {
                matches!(signo.default_action(), DefaultSignalAction::Ignore)
            }
            _ => false,
        }
    }

    /// Checks if syscalls interrupted by the given signal can be restarted.
    pub fn can_restart(&self, signo: Signo) -> bool {
        self.actions.lock()[signo]
            .flags
            .contains(SignalActionFlags::RESTART)
    }

    /// Sends a signal to the process.
    ///
    /// Returns `Some(tid)` if the signal wakes up a thread.
    ///
    /// See [`ThreadSignalManager::send_signal`] for the thread-level version.
    #[must_use]
    pub fn send_signal(&self, sig: SignalInfo) -> Option<u32> {
        let signo = sig.signo();
        // Only discard signals that have no side effects AND are ignored.
        // SIGCONT and SIGKILL must always be queued for their side effects.
        if self.signal_ignored(signo) {
            return None;
        }

        if self.pending.lock().put_signal(sig) {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
        let mut result = None;
        self.children.lock().retain(|(tid, thread)| {
            if let Some(thread) = thread.upgrade() {
                if result.is_none() && !thread.signal_blocked(signo) {
                    result = Some(*tid);
                }
                true
            } else {
                false
            }
        });
        result
    }

    /// Gets currently pending signals.
    pub fn pending(&self) -> SignalSet {
        self.pending.lock().set
    }

    /// Removes a signal from the process pending queue.
    pub fn remove_signal(&self, signo: Signo) {
        self.pending.lock().remove_signal(signo);
    }

    /// Determine whether there is a specific signal pending for the process
    pub fn has_signal(&self, signo: Signo) -> bool {
        self.pending.lock().has_signal(signo)
    }

    /// Clear all stopping signals in the process pending queue if any,
    /// including `SIGSTOP`, `SIGTSTP`, `SIGTTIN`, and `SIGTTOU`.
    pub fn flush_stop_signals(&self) {
        let stop_signals: Vec<Signo> = Signo::iter()
            .filter(|s| matches!(s.default_action(), DefaultSignalAction::Stop))
            .collect();

        let mut pending = self.pending.lock();
        for sig in stop_signals {
            pending.remove_signal(sig);
        }
    }

    /// Records a stop signal effect (atomically).
    ///
    /// This method is called by `do_stop()` when a stop signal
    /// (SIGSTOP, SIGTSTP, SIGTTIN, or SIGTTOU) takes effect on the process:
    /// 1. Records which signal caused the stop, stored in `last_stop_signal`
    /// 2. Sets the `PENDING_STOP_EVENT` flag, for wait to detect it
    ///
    /// # Memory Ordering
    ///
    /// Uses `Release` ordering to synchronize with `Acquire` loads in
    /// `peek_pending_stop_event()`. This ensures that when wait() observes
    /// the stop event, it also observes the signal value.
    ///
    /// # Arguments
    ///
    /// * `signal` - The stop signal that caused the process to stop
    pub fn set_stop_signal(&self, signal: Signo) {
        *self.last_stop_signal.lock() = Some(signal);

        // Set STOP event flag without clearing CONT event
        self.signal_events.fetch_or(
            SignalEventFlags::PENDING_STOP_EVENT.bits(),
            Ordering::Release,
        );
    }

    /// Records a continue signal effect (atomically).
    ///
    /// This method should be called by `do_continue()` when a SIGCONT signal
    /// takes effect on the process:
    /// 1. Clears the recorded stop signal.
    /// 2. Sets the `PENDING_CONT_EVENT` flag for `wait` to detect it.
    ///
    /// # Memory Ordering
    ///
    /// Uses `Release` ordering to synchronize with `Acquire` loads in
    /// `peek_pending_cont_event()`.
    pub fn set_cont_signal(&self) {
        *self.last_stop_signal.lock() = None;

        // Set CONT event flag without clearing STOP event
        self.signal_events.fetch_or(
            SignalEventFlags::PENDING_CONT_EVENT.bits(),
            Ordering::Release,
        );
    }

    /// Peeks at a pending stop signal event without consuming it.
    ///
    /// This method checks if there is an unreported stop event and returns
    /// the signal that caused it. The event remains pending until explicitly
    /// consumed by `consume_stop_event`.
    ///
    /// # Returns
    ///
    /// * `Some(signal)` - There is a pending stop event caused by `signal`
    /// * `None` - No pending stop event (either already consumed or never
    ///   occurred)
    ///
    /// # Memory Ordering
    ///
    /// Uses `Acquire` ordering to synchronize with `Release` stores in
    /// `set_stop_signal`. This ensures that if we see the `PENDING_STOP_EVENT`
    /// flag, we also see the signal value.
    pub fn peek_pending_stop_event(&self) -> Option<Signo> {
        let flags = self.signal_events.load(Ordering::Acquire);

        if (flags & SignalEventFlags::PENDING_STOP_EVENT.bits()) != 0 {
            *self.last_stop_signal.lock()
        } else {
            None
        }
    }

    /// Consumes (clears) the pending stop signal event.
    ///
    /// This method could be called by `wait()` after successfully reporting
    /// the stop event to the waiter, unless `WNOWAIT` is set, which reflects
    /// the "one-time consumption" semantics of POSIX wait:
    /// once consumed, the same stop event will not be reported again.
    ///
    /// # Memory Ordering
    ///
    /// Uses `Release` ordering to ensure that the consumption is visible
    /// to other threads that might be calling `peek_pending_stop_event`.
    pub fn consume_stop_event(&self) {
        *self.last_stop_signal.lock() = None;

        self.signal_events.fetch_and(
            !SignalEventFlags::PENDING_STOP_EVENT.bits(),
            Ordering::Release,
        );
    }

    /// Peeks at a pending continue signal event without consuming it.
    ///
    /// This method checks if there is an unreported SIGCONT event. The event
    /// remains pending until explicitly consumed by `consume_cont_event`.
    ///
    /// # Returns
    ///
    /// * `true` - There is a pending continue event
    /// * `false` - No pending continue event
    ///
    /// # Memory Ordering
    ///
    /// Uses `Acquire` ordering to synchronize with `Release` stores in
    /// `set_cont_signal`.
    pub fn peek_pending_cont_event(&self) -> bool {
        let flags = self.signal_events.load(Ordering::Acquire);
        (flags & SignalEventFlags::PENDING_CONT_EVENT.bits()) != 0
    }

    /// Consumes (clears) the pending continue signal event.
    ///
    /// This method should be called by `wait()` after successfully reporting
    /// the continue event to the waiter, unless `WNOWAIT` is set.
    ///
    /// This implements the "one-time consumption" semantics of POSIX wait:
    /// once consumed, the same continue event will not be reported again.
    ///
    /// # Memory Ordering
    ///
    /// Uses `Release` ordering to ensure that the consumption is visible
    /// to other threads that might be calling `peek_pending_cont_event`.
    pub fn consume_cont_event(&self) {
        self.signal_events.fetch_and(
            !SignalEventFlags::PENDING_CONT_EVENT.bits(),
            Ordering::Release,
        );
    }
}
