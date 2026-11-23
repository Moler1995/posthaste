//! Test-only ownership of the process-wide interface state.
//!
//! The interface language, the palette, and the editor font size are
//! process-wide: every window reads them at paint time, and the tests of one
//! binary share one process, so a change made by one test is a change in every
//! other test that runs beside it. Most tests click controls by the position
//! they painted, and a frame painted at one language or font is not the frame
//! a parallel test expects to click.
//!
//! A test that changes one of those values takes an [`InterfaceGuard`]. While
//! it is held, the setters ignore every other test's writes, so the test that
//! owns the values sees the frame it painted. Tests that never touch these
//! values need no guard: the values stay whatever the owner last set, and each
//! test renders and clicks in that same state.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The one lock every test that changes the globals takes.
static LOCK: Mutex<()> = Mutex::new(());

/// Whether some test currently owns the globals.
static OWNED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Whether this test, rather than another one, owns the globals.
    static OWNS: Cell<bool> = const { Cell::new(false) };
}

/// Whether the caller may change the process-wide interface state.
///
/// Without an owner every caller may, which is the production behaviour.
/// While a test owns the state, only that test's own thread may change it: the
/// app applies its stored preferences on boot, so an unrelated test would
/// otherwise switch the interface out from under a layout another test is
/// clicking through.
pub(crate) fn may_change() -> bool {
    !OWNED.load(Ordering::Acquire) || OWNS.with(|owns| owns.get())
}

/// Serializes the tests that change the interface globals and gives the holder
/// the exclusive right to change them until it drops.
pub(crate) struct InterfaceGuard {
    _lock: MutexGuard<'static, ()>,
}

impl InterfaceGuard {
    /// Waits for the previous owner, takes the globals, and restores the
    /// default language the rest of the tests expect to find.
    pub(crate) fn take() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        OWNS.with(|owns| owns.set(true));
        OWNED.store(true, Ordering::Release);
        crate::i18n::set_language(crate::i18n::Lang::English);
        Self { _lock: lock }
    }
}

impl Drop for InterfaceGuard {
    fn drop(&mut self) {
        crate::i18n::set_language(crate::i18n::Lang::English);
        OWNS.with(|owns| owns.set(false));
        OWNED.store(false, Ordering::Release);
    }
}
