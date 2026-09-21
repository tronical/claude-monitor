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
use log::{error, info, warn};
use mbedtls_rs::{
    Certificate, ClientSessionConfig, Session, SessionConfig, SessionError, Tls, TlsReference,
    X509,
};
use static_cell::StaticCell;

use crate::config::Credentials;
use crate::setup::{self, AccessPoint};
use crate::state::{self, Link, Problem};
use crate::storage::Store;
use crate::usage::{self, Outcome};

/// What the network core should do, decided at boot from what is in flash.
pub enum Mode {
    /// The token in here is only ever written into the TLS session: never
    /// logged, never shown on the display.
    Run(&'static Credentials),
    Setup(&'static AccessPoint),
}

const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// A failed poll is retried sooner than a good one is repeated.
const RETRY_INTERVAL: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
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
        Mode::Setup(access_point) => {
            setup::run(spawner, wifi, trng, access_point, store).await
        }
    };
    spawner.spawn(reconfigure_task(store).unwrap());

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
async fn reconfigure_task(mut store: Store) {
    state::RECONFIGURE.wait().await;
    info!("Reconfiguration requested");
    if !state::halt_ui_core() {
        warn!("UI core did not stop; writing anyway");
    }
    let _ = store.request_setup();
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

    loop {
        state::update(|s| s.link = Link::Connecting);
        info!("Connecting to WiFi network '{}'", credentials.ssid);
        match controller.connect_async().await {
            Ok(_) => {
                info!("WiFi associated");
                state::update(|s| s.link = Link::NoAddress);
                if let Err(e) = controller.wait_for_disconnect_async().await {
                    warn!("WiFi disconnect wait failed: {e:?}");
                }
                warn!("WiFi disconnected");
            }
            Err(e) => warn!("WiFi connect failed: {e:?}"),
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

async fn poll_loop(
    stack: Stack<'static>,
    tls: TlsReference<'static>,
    credentials: &'static Credentials,
) -> ! {
    loop {
        stack.wait_config_up().await;
        if let Some(config) = stack.config_v4() {
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
                info!(
                    "Usage: session {}% weekly {}% ({} ms)",
                    reading.session_pct,
                    reading.weekly_pct,
                    started.elapsed().as_millis()
                );
                state::update(|s| {
                    s.reading = Some((reading, Instant::now()));
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
