//! Standalone Claude subscription usage display for the ESP32-S3-BOX-3.
//!
//! The two cores have one job each. The first runs Slint's event loop from the
//! board support crate, which busy-polls touch and never yields. The second
//! runs an embassy executor with WiFi, the IP stack and the HTTPS poll. They
//! meet only in [`state`].

#![no_std]
#![no_main]

extern crate alloc;

mod config;
mod net;
mod setup;
mod state;
mod storage;
mod usage;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use core::cell::Cell;

use embassy_time::Instant;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::system::Stack;
use esp_hal::timer::timg::TimerGroup;
use log::{debug, info};
use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};
use slint::{ComponentHandle, Image, Rgb8Pixel, SharedPixelBuffer};
use static_cell::StaticCell;

use crate::config::{Network, Settings, wifi_qr_escape};
use crate::net::Mode;
use crate::setup::AccessPoint;
use crate::state::{Link, Problem, SetupStage, Snapshot};
use crate::storage::{Store, Stored};

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

    // Settings saved by the setup form win over ones baked in at build time,
    // and an explicit "set up again" wins over both.
    let mut store = Store::new(peripherals.FLASH);
    let built_in = Settings {
        networks: alloc::vec![Network {
            ssid: env!("WIFI_SSID").into(),
            password: env!("WIFI_PASSWORD").into(),
        }],
        token: env!("CLAUDE_OAUTH_TOKEN").into(),
    };
    let mut previous = None;
    let settings = match store.load() {
        Stored::Settings(stored) => Some(stored),
        Stored::SetupRequested { previous: kept } => {
            previous = kept;
            None
        }
        Stored::Nothing => built_in.validate().is_ok().then_some(built_in),
    };
    let access_point: Option<&'static AccessPoint> = match settings {
        Some(_) => None,
        None => Some(Box::leak(Box::new(AccessPoint::generate(trng)))),
    };
    let mode = match (settings, access_point) {
        (Some(settings), _) => {
            info!("Using settings with {} stored network(s)", settings.networks.len());
            Mode::Run(Box::leak(Box::new(settings)))
        }
        (None, Some(access_point)) => {
            info!("No settings: starting setup as '{}'", access_point.ssid);
            Mode::Setup { access_point, previous }
        }
        (None, None) => unreachable!(),
    };

    // A reading carried across the WiFi recovery reset, if this is one.
    if let Some(held) = net::recovery::take_reading() {
        info!("Recovered the last reading from before the reset");
        state::update(|s| s.reading = Some(held));
    }

    let wifi = peripherals.WIFI;
    static NETWORK_STACK: StaticCell<Stack<NETWORK_CORE_STACK>> = StaticCell::new();
    esp_rtos::start_second_core(
        peripherals.CPU_CTRL,
        software_interrupts.software_interrupt1,
        NETWORK_STACK.init(Stack::new()),
        move || {
            static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();
            EXECUTOR.init(esp_rtos::embassy::Executor::new()).run(|spawner| {
                spawner.spawn(net::run(spawner, wifi, trng, store, mode).unwrap());
            });
        },
    );
    debug!("Network core started");

    let window = MainWindow::new().expect("creating the window");
    window.on_refresh(|| state::REFRESH.signal(()));
    window.on_reconfigure(|keep_token| {
        state::RECONFIGURE.signal(if keep_token {
            state::Reconfigure::WifiOnly
        } else {
            state::Reconfigure::Everything
        })
    });
    let setup_screen = access_point.map(SetupScreen::new);

    // The network core cannot call into Slint, so the UI pulls instead. Twice
    // a second is plenty for minute-granular countdowns.
    let refresh_timer = slint::Timer::default();
    let weak_window = window.as_weak();
    refresh_timer.start(
        slint::TimerMode::Repeated,
        core::time::Duration::from_millis(500),
        move || {
            state::halt_here_if_requested();
            if let Some(window) = weak_window.upgrade() {
                match &setup_screen {
                    Some(setup_screen) => setup_screen.present(&window, &state::snapshot()),
                    None => present(&window, &state::snapshot()),
                }
            }
        },
    );

    window.run().expect("running the event loop");
    unreachable!("the board support event loop never returns")
}

/// What the setup screen shows at each stage. The QR codes are rendered once,
/// up front: they only depend on the access point chosen at boot.
struct SetupScreen {
    access_point: &'static AccessPoint,
    join_qr: Image,
    portal_qr: Image,
    shown: Cell<Option<SetupStage>>,
}

impl SetupScreen {
    fn new(access_point: &'static AccessPoint) -> Self {
        let join = format!(
            "WIFI:T:WPA;S:{};P:{};;",
            wifi_qr_escape(&access_point.ssid),
            wifi_qr_escape(&access_point.password)
        );
        Self {
            access_point,
            join_qr: qr_image(&join),
            portal_qr: qr_image(setup::PORTAL_URL),
            shown: Cell::new(None),
        }
    }

    fn present(&self, window: &MainWindow, snapshot: &Snapshot) {
        let Link::Setup(stage) = snapshot.link else {
            return;
        };
        // Replacing the image repaints the whole code; only do it on a change.
        if self.shown.replace(Some(stage)) == Some(stage) {
            return;
        }

        let AccessPoint { ssid, password } = self.access_point;
        let (qr, step, headline, detail) = match stage {
            SetupStage::Starting => {
                (None, "SETUP", "Starting", String::from("Looking for WiFi networks nearby."))
            }
            SetupStage::WaitingForClient => (
                Some(&self.join_qr),
                "STEP 1 OF 2",
                "Scan to join the setup WiFi",
                format!("Network\n{ssid}\n\nPassword\n{password}"),
            ),
            SetupStage::ClientJoined => (
                Some(&self.portal_qr),
                "STEP 2 OF 2",
                "Scan to open the setup page",
                String::from("It may have opened on your phone already.\n\nOr browse to 192.168.4.1"),
            ),
            SetupStage::FormOpened => (
                Some(&self.portal_qr),
                "STEP 2 OF 2",
                "Fill in the form on your phone",
                String::from("Closed it? Scan again, or browse to 192.168.4.1"),
            ),
            SetupStage::Saving => {
                (None, "DONE", "Saving", String::from("Restarting to join your WiFi."))
            }
        };

        window.set_setup_mode(true);
        window.set_setup_qr_visible(qr.is_some());
        if let Some(qr) = qr {
            window.set_setup_qr(qr.clone());
        }
        window.set_setup_step(step.into());
        window.set_setup_headline(headline.into());
        window.set_setup_detail(detail.into());
    }
}

/// Render `text` as a QR code sized for the 164 px card on the setup screen.
fn qr_image(text: &str) -> Image {
    // Version 10 holds a couple of hundred bytes; the payloads are under 60.
    const MAX_VERSION: Version = Version::new(10);
    const BUFFER_LEN: usize = MAX_VERSION.buffer_len();
    /// The card around the code supplies the rest of the quiet zone.
    const QUIET_MODULES: u32 = 1;
    const TARGET_PX: u32 = 156;

    let mut scratch = [0u8; BUFFER_LEN];
    let mut modules = [0u8; BUFFER_LEN];
    let qr = QrCode::encode_text(
        text,
        &mut scratch,
        &mut modules,
        QrCodeEcc::Medium,
        Version::MIN,
        MAX_VERSION,
        None,
        true,
    )
    .expect("setup QR payloads fit a version 10 code");

    let size = qr.size() as u32;
    // A whole number of pixels per module: a resampled QR code does not scan.
    let scale = (TARGET_PX / (size + 2 * QUIET_MODULES)).max(1);
    let side = (size + 2 * QUIET_MODULES) * scale;

    let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(side, side);
    for (index, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (index as u32 % side, index as u32 / side);
        let module_x = (x / scale) as i32 - QUIET_MODULES as i32;
        let module_y = (y / scale) as i32 - QUIET_MODULES as i32;
        // Out-of-range modules read as light, which is the quiet zone.
        let shade = if qr.get_module(module_x, module_y) { 0 } else { 255 };
        *pixel = Rgb8Pixel::new(shade, shade, shade);
    }
    Image::from_rgb8(pixels)
}

/// Push a snapshot into the window's properties.
fn present(window: &MainWindow, snapshot: &Snapshot) {
    window.set_online(snapshot.link == Link::Online);
    window.set_link_text(
        match snapshot.link {
            Link::Setup(_) => "SETUP",
            Link::Connecting => "CONNECTING",
            Link::NoKnownNetwork => "NO KNOWN WIFI",
            Link::NoAddress => "NO ADDRESS",
            Link::Online if snapshot.polling => "UPDATING",
            Link::Online => "ONLINE",
        }
        .into(),
    );

    let Some(held) = snapshot.reading else {
        window.set_usage_known(false);
        window.set_warning(snapshot.problem.is_some());
        window.set_summary(
            match (snapshot.link, snapshot.problem) {
                (_, Some(problem)) => describe(problem),
                (Link::Online, None) => "FETCHING USAGE".into(),
                (Link::NoKnownNetwork, None) => "NO KNOWN WIFI HERE · HOLD TO ADD ONE".into(),
                (_, None) if Instant::now().as_secs() > 45 => "NO WIFI · HOLD SCREEN TO SET UP".into(),
                (_, None) => "WAITING FOR WIFI".into(),
            }
            .into(),
        );
        return;
    };

    let reading = held.reading;
    let age = held.age_secs();
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

    let (summary, warning) = summarize(snapshot, &reading, age, session_remaining);
    window.set_summary(summary.into());
    window.set_warning(warning);
}

/// The one line under the meters, and whether it is bad news.
fn summarize(
    snapshot: &Snapshot,
    reading: &usage::Reading,
    age: u64,
    session_remaining: Option<u64>,
) -> (String, bool) {
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
