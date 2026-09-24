//! Everything that runs on the second core: WiFi, the IP stack, and the
//! once-a-minute HTTPS probe that reads usage out of the response headers.

use alloc::string::String;
use alloc::vec;
use core::ffi::CStr;
use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::peripherals::WIFI;
use esp_hal::rng::Trng;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{Config as WifiConfig, Interface, WifiController};
use log::{debug, error, info, warn};
use mbedtls_rs::{
    Certificate, ClientSessionConfig, Session, SessionConfig, SessionError, Tls, TlsReference,
    X509,
};
use static_cell::StaticCell;

use crate::config::Credentials;
use crate::setup::{self, AccessPoint};
use crate::state::{self, Held, Link, Problem, Reconfigure};
use crate::storage::Store;
use crate::usage::{self, Outcome};

/// What the network core should do, decided at boot from what is in flash.
pub enum Mode {
    /// The token in here is only ever written into the TLS session: never
    /// logged, never shown on the display.
    Run(&'static Credentials),
    Setup {
        access_point: &'static AccessPoint,
        /// Settings to start the form from, when only the WiFi is changing.
        previous: Option<Credentials>,
    },
}

const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// A failed poll is retried sooner than a good one is repeated.
const RETRY_INTERVAL: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Connect attempts failing continuously for this long get the whole box
/// reset. Seen in the field: after some disconnect the driver answers every
/// reconnect with `NoAccessPointFound` while the network is plainly there,
/// and nothing short of re-initialising it (which esp-radio 0.18 offers no
/// call for) brings it back. A reset does exactly that in a second or two.
const CONNECT_FAILURE_RESET_AFTER: Duration = Duration::from_secs(120);
/// Taps closer together than this are ignored; every poll is a real request
/// against the subscription.
const MIN_POLL_SPACING: Duration = Duration::from_secs(5);

const API_HOST_C: &CStr = c"api.anthropic.com";

/// The roots api.anthropic.com can chain to today (Google Trust Services),
/// plus the ISRG roots as a hedge against a CA change. See `certs/README.md`.
const CA_BUNDLE: &CStr =
    match CStr::from_bytes_with_nul(concat!(include_str!("../certs/roots.pem"), "\0").as_bytes()) {
        Ok(bundle) => bundle,
        Err(_) => panic!("certs/roots.pem contains a NUL byte"),
    };

/// Bring the network up and poll forever. Spawned once, on the second core.
#[embassy_executor::task]
pub async fn run(
    spawner: Spawner,
    wifi: WIFI<'static>,
    trng: &'static mut Trng,
    store: Store,
    mode: Mode,
) {
    let credentials = match mode {
        Mode::Run(credentials) => credentials,
        Mode::Setup { access_point, previous } => {
            setup::run(spawner, wifi, trng, access_point, previous, store).await
        }
    };
    spawner.spawn(reconfigure_task(store, credentials).unwrap());

    // The controller is created here rather than on the first core because
    // esp-radio pins its WiFi task to whichever core calls this.
    let (controller, interfaces) = match esp_radio::wifi::new(wifi, Default::default()) {
        Ok(wifi) => wifi,
        Err(e) => {
            error!("WiFi init failed: {e:?}");
            return;
        }
    };

    let seed = u64::from(trng.random()) << 32 | u64::from(trng.random());
    static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    static TLS: StaticCell<Tls<'static>> = StaticCell::new();
    let tls = match Tls::new(trng) {
        Ok(tls) => TLS.init(tls),
        Err(e) => {
            error!("TLS init failed: {e:?}");
            return;
        }
    };

    spawner.spawn(connection_task(controller, credentials).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    poll_loop(stack, tls.reference(), credentials).await
}

/// "Reconfigure" from the UI: leave a marker in flash and restart into setup.
#[embassy_executor::task]
async fn reconfigure_task(mut store: Store, credentials: &'static Credentials) {
    let scope = state::RECONFIGURE.wait().await;
    info!("Reconfiguration requested: {scope:?}");
    if !state::halt_ui_core() {
        warn!("UI core did not stop; writing anyway");
    }
    let keep = match scope {
        Reconfigure::WifiOnly => Some(credentials),
        Reconfigure::Everything => None,
    };
    let _ = store.request_setup(keep);
    // The UI core is gone either way, so restarting is the only way forward.
    esp_hal::system::software_reset();
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// Keep the station associated, reconnecting for as long as it takes.
#[embassy_executor::task]
async fn connection_task(
    mut controller: WifiController<'static>,
    credentials: &'static Credentials,
) {
    let config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(credentials.ssid.as_str())
            .with_password(credentials.password.clone()),
    );
    if let Err(e) = controller.set_config(&config) {
        error!("WiFi configuration rejected: {e:?}");
        return;
    }

    let mut failing_since: Option<Instant> = None;
    loop {
        state::update(|s| s.link = Link::Connecting);
        info!("Connecting to WiFi network '{}'", credentials.ssid);
        match controller.connect_async().await {
            Ok(_) => {
                failing_since = None;
                debug!("WiFi associated");
                state::update(|s| s.link = Link::NoAddress);
                if let Err(e) = controller.wait_for_disconnect_async().await {
                    warn!("WiFi disconnect wait failed: {e:?}");
                }
                warn!("WiFi disconnected");
            }
            Err(e) => {
                warn!("WiFi connect failed: {e:?}");
                let since = *failing_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= CONNECT_FAILURE_RESET_AFTER {
                    warn!("WiFi has not come back; resetting to re-initialise the driver");
                    recovery::stash_reading();
                    esp_hal::system::software_reset();
                }
            }
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

/// Carrying the last reading across the recovery reset, so the display keeps
/// its numbers and countdowns instead of going blank for the reboot.
pub mod recovery {
    use embassy_time::Instant;

    use crate::state::{self, Held};
    use crate::usage::Reading;

    const MAGIC: u32 = 0x5245_4144; // "READ"
    const NONE: u32 = u32::MAX;

    /// RTC fast memory survives a software reset. Zeroed on power-up only, so
    /// a record is cleared once it has been picked up; the checksum covers a
    /// reset landing in the middle of a write.
    #[esp_hal::ram(unstable(rtc_fast, persistent))]
    static mut STASH: [u32; 8] = [0; 8];

    pub fn stash_reading() {
        let Some(held) = state::snapshot().reading else {
            return;
        };
        let Reading { session_pct, weekly_pct, session_reset_in, weekly_reset_in, limited } =
            held.reading;
        let age = held.age_secs().min(u32::MAX as u64) as u32;
        let mut words = [
            MAGIC,
            u32::from(session_pct),
            u32::from(weekly_pct),
            session_reset_in.unwrap_or(NONE),
            weekly_reset_in.unwrap_or(NONE),
            u32::from(limited),
            age,
            0,
        ];
        words[7] = checksum(&words[..7]);
        // SAFETY: the network core is the only writer, and it is about to reset
        // the chip; the reader runs at the next boot, before any task exists.
        unsafe { STASH = words };
    }

    /// Take a stashed reading, if this boot follows a recovery reset.
    pub fn take_reading() -> Option<Held> {
        // SAFETY: called once, at boot, before the second core is started.
        let words = unsafe { STASH };
        unsafe { STASH = [0; 8] };
        if words[0] != MAGIC || words[7] != checksum(&words[..7]) {
            return None;
        }
        let reset_in = |word: u32| (word != NONE).then_some(word);
        let reading = Reading {
            session_pct: words[1].min(100) as u8,
            weekly_pct: words[2].min(100) as u8,
            session_reset_in: reset_in(words[3]),
            weekly_reset_in: reset_in(words[4]),
            limited: words[5] != 0,
        };
        // Plus the reboot itself, which the monotonic clock did not see.
        let age = u64::from(words[6]) + Instant::now().as_secs();
        Some(Held::carried(reading, age))
    }

    fn checksum(words: &[u32]) -> u32 {
        words.iter().fold(0x811C_9DC5u32, |hash, &word| {
            (hash ^ word).wrapping_mul(0x0100_0193)
        })
    }
}

async fn poll_loop(
    stack: Stack<'static>,
    tls: TlsReference<'static>,
    credentials: &'static Credentials,
) -> ! {
    loop {
        stack.wait_config_up().await;
        // Once per connection, not once per poll.
        if state::snapshot().link != Link::Online
            && let Some(config) = stack.config_v4()
        {
            info!("Got address {}", config.address);
        }
        state::update(|s| {
            s.link = Link::Online;
            s.polling = true;
        });

        let started = Instant::now();
        let result = match with_timeout(REQUEST_TIMEOUT, probe(stack, tls, &credentials.token)).await {
            Ok(result) => result,
            Err(_) => Err(Problem::Timeout),
        };

        let wait = match result {
            Ok(reading) => {
                debug!(
                    "Usage: session {}% weekly {}% ({} ms)",
                    reading.session_pct,
                    reading.weekly_pct,
                    started.elapsed().as_millis()
                );
                state::update(|s| {
                    s.reading = Some(Held::fresh(reading));
                    s.problem = None;
                    s.polling = false;
                });
                POLL_INTERVAL
            }
            Err(problem) => {
                warn!("Poll failed: {problem:?}");
                state::update(|s| {
                    s.problem = Some(problem);
                    s.polling = false;
                });
                match problem {
                    // Hammering the API with a bad token helps nobody.
                    Problem::Unauthorized | Problem::NoLimits => POLL_INTERVAL * 5,
                    _ => RETRY_INTERVAL,
                }
            }
        };

        Timer::after(MIN_POLL_SPACING).await;
        state::REFRESH.reset();
        let remaining = wait.checked_sub(MIN_POLL_SPACING).unwrap_or(Duration::MIN);
        embassy_futures::select::select(
            Timer::after(remaining),
            embassy_futures::select::select(state::REFRESH.wait(), wait_link_down(stack)),
        )
        .await;
    }
}

/// Resolves when DHCP state is lost, so the display stops claiming ONLINE.
async fn wait_link_down(stack: Stack<'static>) {
    stack.wait_config_down().await;
    // `connection_task` moves the state on from here once it notices.
    state::update(|s| s.link = Link::Connecting);
}

/// One HTTPS request; only the response head is ever read.
async fn probe(
    stack: Stack<'static>,
    tls: TlsReference<'_>,
    token: &str,
) -> Result<usage::Reading, Problem> {
    let address = stack
        .dns_query(usage::API_HOST, DnsQueryType::A)
        .await
        .ok()
        .and_then(|addresses| addresses.first().copied())
        .ok_or(Problem::Dns)?;

    let mut rx_buffer = vec![0u8; 4096];
    let mut tx_buffer = vec![0u8; 2048];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(15)));
    socket.connect((address, 443)).await.map_err(|_| Problem::Connect)?;

    let config = ClientSessionConfig {
        ca_chain: Some(Certificate::new(X509::PEM(CA_BUNDLE)).map_err(|_| Problem::Tls)?),
        server_name: Some(API_HOST_C),
        ..ClientSessionConfig::new()
    };
    let mut session =
        Session::new(tls, &mut socket, &SessionConfig::Client(config)).map_err(|_| Problem::Tls)?;
    session.connect().await.map_err(|e| {
        warn!("TLS handshake failed: {e:?}");
        Problem::Tls
    })?;

    let mut request = String::with_capacity(512 + token.len());
    // The token is an OAuth token, so it goes on `Authorization: Bearer` with
    // the OAuth beta header, not on `x-api-key`.
    let _ = write!(
        request,
        "POST /v1/messages HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bearer {token}\r\n\
         anthropic-version: 2023-06-01\r\n\
         anthropic-beta: oauth-2025-04-20\r\n\
         User-Agent: claude-monitor/{version}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {length}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        host = usage::API_HOST,
        version = env!("CARGO_PKG_VERSION"),
        length = usage::PROBE_BODY.len(),
        body = usage::PROBE_BODY,
    );
    write_all(&mut session, request.as_bytes()).await.map_err(|_| Problem::Connect)?;
    session.flush().await.map_err(|_| Problem::Connect)?;

    let mut head = vec![0u8; 8192];
    let mut filled = 0;
    let head_end = loop {
        if let Some(end) = find_head_end(&head[..filled]) {
            break end;
        }
        if filled == head.len() {
            return Err(Problem::BadResponse);
        }
        match session.read(&mut head[filled..]).await {
            Ok(0) | Err(_) => return Err(Problem::BadResponse),
            Ok(n) => filled += n,
        }
    };
    // The body is a completion nobody wants; drop the connection instead of
    // draining it.
    let _ = session.close().await;

    let head = core::str::from_utf8(&head[..head_end]).map_err(|_| Problem::BadResponse)?;
    match usage::parse_response_head(head) {
        Outcome::Reading(reading) => Ok(reading),
        Outcome::NoLimits => Err(Problem::NoLimits),
        Outcome::Unauthorized => Err(Problem::Unauthorized),
        Outcome::Http(status) => Err(Problem::Http(status)),
        Outcome::Malformed => Err(Problem::BadResponse),
    }
}

async fn write_all<T>(session: &mut Session<'_, T>, mut data: &[u8]) -> Result<(), SessionError>
where
    T: mbedtls_rs::io::Read + mbedtls_rs::io::Write,
{
    while !data.is_empty() {
        let written = session.write(data).await?;
        data = &data[written..];
    }
    Ok(())
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}
