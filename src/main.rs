//! Standalone Claude subscription usage display for the ESP32-S3-BOX-3.
//!
//! The two cores have one job each. The first runs Slint's event loop from the
//! board support crate, which busy-polls touch and never yields. The second
//! runs an embassy executor with WiFi, the IP stack and the HTTPS poll. They
//! meet only in [`state`].

#![no_std]
#![no_main]

extern crate alloc;

mod net;
mod state;
mod usage;

use alloc::format;
use alloc::string::String;

use embassy_time::Instant;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::system::Stack;
use esp_hal::timer::timg::TimerGroup;
use log::info;
use slint::ComponentHandle;
use static_cell::StaticCell;

use crate::state::{Link, Problem, Snapshot};

slint::include_modules!();

const SESSION_WINDOW_SECS: u32 = 5 * 60 * 60;
const WEEKLY_WINDOW_SECS: u32 = 7 * 24 * 60 * 60;
/// Two missed polls make the numbers stale.
const STALE_AFTER_SECS: u64 = 150;

/// TLS handshakes with P-384 roots are deep; mbedtls runs on this stack.
const NETWORK_CORE_STACK: usize = 96 * 1024;

#[mcu_board_support::entry]
fn main() -> ! {
    // Sets up the display, touch, the Slint platform and the PSRAM heap.
    mcu_board_support::init();

    // The radio needs internal RAM, which the board support does not put on
    // the heap. Registered after PSRAM on purpose: esp-alloc serves ordinary
    // allocations from the first region with room, so Slint and TLS land in
    // PSRAM and these stay free for the WiFi driver, which asks for internal
    // memory explicitly.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 96 * 1024);

    // SAFETY: the board support's `init` took the peripherals, used the ones
    // for display and touch (SPI2, I2C0, PSRAM, a few GPIOs) and dropped the
    // rest. Only peripherals it never touches are used from this second set.
    let peripherals = unsafe { esp_hal::peripherals::Peripherals::steal() };

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let software_interrupts = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupts.software_interrupt0);

    // Keeps the hardware RNG seeded from real entropy for as long as it lives,
    // which is forever.
    static TRNG_SOURCE: StaticCell<TrngSource<'static>> = StaticCell::new();
    TRNG_SOURCE.init(TrngSource::new(peripherals.RNG, peripherals.ADC1));
    static TRNG: StaticCell<Trng> = StaticCell::new();
    let trng = TRNG.init(Trng::try_new().expect("TRNG source is alive"));

    let wifi = peripherals.WIFI;
    static NETWORK_STACK: StaticCell<Stack<NETWORK_CORE_STACK>> = StaticCell::new();
    esp_rtos::start_second_core(
        peripherals.CPU_CTRL,
        software_interrupts.software_interrupt1,
        NETWORK_STACK.init(Stack::new()),
        move || {
            static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();
            EXECUTOR.init(esp_rtos::embassy::Executor::new()).run(|spawner| {
                spawner.spawn(net::run(spawner, wifi, trng).unwrap());
            });
        },
    );
    info!("Network core started");

    let window = MainWindow::new().expect("creating the window");
    window.on_refresh(|| state::REFRESH.signal(()));

    // The network core cannot call into Slint, so the UI pulls instead. Twice
    // a second is plenty for minute-granular countdowns.
    let refresh_timer = slint::Timer::default();
    let weak_window = window.as_weak();
    refresh_timer.start(
        slint::TimerMode::Repeated,
        core::time::Duration::from_millis(500),
        move || {
            if let Some(window) = weak_window.upgrade() {
                present(&window, &state::snapshot());
            }
        },
    );
    present(&window, &state::snapshot());

    window.run().expect("running the event loop");
    unreachable!("the board support event loop never returns")
}

/// Push a snapshot into the window's properties.
fn present(window: &MainWindow, snapshot: &Snapshot) {
    window.set_online(snapshot.link == Link::Online);
    window.set_link_text(
        match snapshot.link {
            Link::NotConfigured => "NO CONFIG",
            Link::Connecting => "CONNECTING",
            Link::NoAddress => "NO ADDRESS",
            Link::Online if snapshot.polling => "UPDATING",
            Link::Online => "ONLINE",
        }
        .into(),
    );

    let Some((reading, received)) = snapshot.reading else {
        window.set_usage_known(false);
        window.set_warning(snapshot.problem.is_some());
        window.set_summary(
            match (snapshot.link, snapshot.problem) {
                (Link::NotConfigured, _) => "FILL IN SECRETS.ENV AND REFLASH".into(),
                (_, Some(problem)) => describe(problem),
                (Link::Online, None) => "FETCHING USAGE".into(),
                (_, None) => "WAITING FOR WIFI".into(),
            }
            .into(),
        );
        return;
    };

    let age = received.elapsed().as_secs();
    let remaining = |at_fetch: Option<u32>| at_fetch.map(|secs| (secs as u64).saturating_sub(age));
    let session_remaining = remaining(reading.session_reset_in);
    let weekly_remaining = remaining(reading.weekly_reset_in);

    window.set_usage_known(true);
    window.set_session_percent(reading.session_pct.into());
    window.set_weekly_percent(reading.weekly_pct.into());
    window.set_session_reset(format_reset(session_remaining).into());
    window.set_weekly_reset(format_reset(weekly_remaining).into());
    window.set_session_elapsed(elapsed_fraction(session_remaining, SESSION_WINDOW_SECS));
    window.set_weekly_elapsed(elapsed_fraction(weekly_remaining, WEEKLY_WINDOW_SECS));

    let (summary, warning) = summarize(snapshot, &reading, received, session_remaining);
    window.set_summary(summary.into());
    window.set_warning(warning);
}

/// The one line under the meters, and whether it is bad news.
fn summarize(
    snapshot: &Snapshot,
    reading: &usage::Reading,
    received: Instant,
    session_remaining: Option<u64>,
) -> (String, bool) {
    let age = received.elapsed().as_secs();
    if let Some(problem @ (Problem::Unauthorized | Problem::NoLimits)) = snapshot.problem {
        return (describe(problem), true);
    }
    if age >= STALE_AFTER_SECS {
        let cause = match snapshot.problem {
            Some(problem) => describe(problem),
            None => "STALE".into(),
        };
        return (format!("{cause} · {}", format_age(age)), false);
    }
    if reading.limited || reading.session_pct >= 100 {
        return ("LIMIT REACHED".into(), true);
    }

    // Project the session to the end of its window at the average rate so far.
    // Too early in the window the average is noise, so say nothing yet.
    let Some(remaining) = session_remaining else {
        return ("TAP TO REFRESH".into(), false);
    };
    let window = u64::from(SESSION_WINDOW_SECS);
    let elapsed = window.saturating_sub(remaining);
    if elapsed < 10 * 60 || reading.session_pct == 0 {
        return ("SESSION JUST STARTED".into(), false);
    }
    let pct = u64::from(reading.session_pct);
    let projected = pct * window / elapsed;
    if projected <= 100 {
        return (format!("ON PACE FOR {projected}% AT RESET"), false);
    }
    // Seconds until 100% at this rate, measured from now.
    let until_limit = (elapsed * 100 / pct).saturating_sub(elapsed);
    (format!("LIMIT IN {} AT THIS PACE", format_span(until_limit)), true)
}

fn describe(problem: Problem) -> String {
    match problem {
        Problem::Dns => "DNS LOOKUP FAILED".into(),
        Problem::Connect => "API UNREACHABLE".into(),
        Problem::Tls => "TLS VERIFICATION FAILED".into(),
        Problem::Timeout => "REQUEST TIMED OUT".into(),
        Problem::Unauthorized => "TOKEN REJECTED · RUN SETUP-TOKEN".into(),
        Problem::NoLimits => "ACCOUNT REPORTS NO LIMITS".into(),
        Problem::Http(status) => format!("API ERROR {status}"),
        Problem::BadResponse => "UNREADABLE RESPONSE".into(),
    }
}

/// How far through a window we are, or negative when that is unknown.
fn elapsed_fraction(remaining: Option<u64>, window_secs: u32) -> f32 {
    match remaining {
        Some(remaining) => {
            let remaining = remaining.min(u64::from(window_secs)) as f32;
            1.0 - remaining / window_secs as f32
        }
        None => -1.0,
    }
}

fn format_reset(remaining: Option<u64>) -> String {
    match remaining {
        None => "reset time unknown".into(),
        Some(0) => "resetting now".into(),
        Some(secs) => format!("resets in {}", format_span(secs)),
    }
}

/// Partial minutes round up: twenty seconds away should still read `1m`.
fn format_span(secs: u64) -> String {
    let minutes = secs.div_ceil(60);
    if minutes >= 24 * 60 {
        format!("{}d {}h", minutes / (24 * 60), minutes % (24 * 60) / 60)
    } else if minutes >= 60 {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

fn format_age(secs: u64) -> String {
    if secs < 60 * 60 {
        format!("{}M AGO", secs / 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}H AGO", secs / (60 * 60))
    } else {
        format!("{}D AGO", secs / (24 * 60 * 60))
    }
}
