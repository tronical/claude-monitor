//! What the network core publishes and the UI core reads.
//!
//! Slint is built `unsafe-single-threaded`, so nothing on the network core may
//! touch it. The two cores only ever meet here: the network side overwrites a
//! small `Copy` snapshot, and a Slint timer on the UI side polls it.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Instant;

use crate::usage::Reading;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// First-run setup; see `setup.rs`.
    Setup(SetupStage),
    Connecting,
    /// Associated, waiting for DHCP.
    NoAddress,
    Online,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupStage {
    /// Scanning for networks and bringing the access point up.
    Starting,
    WaitingForClient,
    /// A phone has joined the access point but not opened the form yet.
    ClientJoined,
    FormOpened,
    /// A valid form came in; it is being written and the device will restart.
    Saving,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Dns,
    Connect,
    /// The TLS handshake failed; most likely the certificate did not verify.
    Tls,
    Timeout,
    Unauthorized,
    NoLimits,
    Http(u16),
    BadResponse,
}

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub link: Link,
    /// The last good reading and when it arrived. Kept across later failures
    /// so the display can keep counting down while saying the data is stale.
    pub reading: Option<(Reading, Instant)>,
    /// Why the most recent poll failed, if it did.
    pub problem: Option<Problem>,
    pub polling: bool,
}

static SNAPSHOT: Mutex<CriticalSectionRawMutex, Cell<Snapshot>> = Mutex::new(Cell::new(Snapshot {
    link: Link::Connecting,
    reading: None,
    problem: None,
    polling: false,
}));

/// Raised by the UI (a tap) to poll now instead of at the next interval.
pub static REFRESH: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Raised by the UI to forget the stored settings and restart into setup.
/// Flash is only ever written from the network core, so this goes through it.
pub static RECONFIGURE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

static UI_HALT_REQUESTED: AtomicBool = AtomicBool::new(false);
static UI_HALTED: AtomicBool = AtomicBool::new(false);

/// Stop the UI core for good, ahead of a flash write and the restart that
/// always follows one. Returns whether it confirmed.
///
/// A flash write stalls the other core wherever it happens to be. If that is
/// inside a critical section, the stalled core keeps the global lock and the
/// writing core deadlocks the moment it wants it. So the UI core is asked to
/// stop by itself first, at a point where it holds nothing.
///
/// Deliberately blocking, and the caller must stay blocking until it resets:
/// the embassy timer interrupt is serviced by the UI core, so once that core
/// stops no `Timer` on this one ever fires again, timeouts included.
pub fn halt_ui_core() -> bool {
    UI_HALT_REQUESTED.store(true, Ordering::SeqCst);
    // Reads a hardware counter; needs no interrupt.
    let started = esp_hal::time::Instant::now();
    while started.elapsed() < esp_hal::time::Duration::from_secs(3) {
        if UI_HALTED.load(Ordering::SeqCst) {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Called by the UI core from its timer, which is such a point: no locks
/// held, no display transfer in flight. Does not return once asked to halt.
pub fn halt_here_if_requested() {
    if !UI_HALT_REQUESTED.load(Ordering::SeqCst) {
        return;
    }
    // An interrupt handler could take the lock just as well as this code, so
    // they go first. Plain register write: no lock involved in turning them off.
    esp_hal::xtensa_lx::interrupt::disable();
    UI_HALTED.store(true, Ordering::SeqCst);
    loop {
        core::hint::spin_loop();
    }
}

pub fn snapshot() -> Snapshot {
    SNAPSHOT.lock(Cell::get)
}

pub fn update(change: impl FnOnce(&mut Snapshot)) {
    SNAPSHOT.lock(|cell| {
        let mut snapshot = cell.get();
        change(&mut snapshot);
        cell.set(snapshot);
    });
}
