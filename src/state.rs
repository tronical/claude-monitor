//! What the network core publishes and the UI core reads.
//!
//! Slint is built `unsafe-single-threaded`, so nothing on the network core may
//! touch it. The two cores only ever meet here: the network side overwrites a
//! small `Copy` snapshot, and a Slint timer on the UI side polls it.

use core::cell::Cell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Instant;

use crate::usage::Reading;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// `secrets.env` was missing or incomplete at build time.
    NotConfigured,
    Connecting,
    /// Associated, waiting for DHCP.
    NoAddress,
    Online,
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
