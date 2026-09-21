//! First-run setup: the box becomes a WiFi access point and serves one form.
//!
//! The screen shows a QR code for joining the access point. A catch-all DNS
//! server and an HTTP redirect make phones pop up their captive-portal sheet
//! with the form on their own; a second QR code covers the ones that do not.
//! The submitted settings go to flash and the device restarts into normal
//! operation.
//!
//! The form travels over plain HTTP, but inside a WPA2 network whose password
//! is random per boot and only ever shown on the device's own screen.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::peripherals::WIFI;
use esp_hal::rng::Trng;
use esp_radio::wifi::ap::AccessPointConfig;
use esp_radio::wifi::{AuthenticationMethod, Config as WifiConfig, WifiController};
use leasehund::{DhcpServer, TransactionEvent};
use log::{debug, info, warn};
use static_cell::StaticCell;

use crate::config::{Credentials, html_escape, url_decode};
use crate::net::net_task;
use crate::state::{self, Link, SetupStage};
use crate::storage::Store;

pub const PORTAL_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
pub const PORTAL_URL: &str = "http://192.168.4.1/";

/// The access point the device opens, decided on the UI core so the QR code
/// can be on screen before the radio is even up.
pub struct AccessPoint {
    pub ssid: String,
    pub password: String,
}

impl AccessPoint {
    pub fn generate(trng: &mut Trng) -> Self {
        // No 0/o, 1/l/i: the password may have to be typed from the screen.
        const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
        // Rejection sampling keeps the draw uniform over the alphabet.
        let draw = || loop {
            let candidate = (trng.random() & 0x1F) as usize;
            if candidate < ALPHABET.len() {
                break ALPHABET[candidate] as char;
            }
        };
        let suffix: String = (0..4).map(|_| draw()).collect();
        let password: String = (0..10).map(|_| draw()).collect();
        Self { ssid: format!("claude-monitor-{suffix}"), password }
    }
}

pub async fn run(
    spawner: Spawner,
    wifi: WIFI<'static>,
    trng: &'static mut Trng,
    access_point: &'static AccessPoint,
    store: Store,
) -> ! {
    state::update(|s| s.link = Link::Setup(SetupStage::Starting));

    let (mut controller, interfaces) =
        esp_radio::wifi::new(wifi, Default::default()).expect("WiFi init");

    // Scanning needs station mode, so it happens once, before the access
    // point comes up. The result only feeds the form's suggestions.
    let networks = scan(&mut controller).await;

    let config = WifiConfig::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(access_point.ssid.as_str())
            .with_password(access_point.password.clone())
            .with_auth_method(AuthenticationMethod::Wpa2Personal)
            .with_max_connections(2),
    );
    controller.set_config(&config).expect("access point configuration");
    info!("Setup access point '{}' is up", access_point.ssid);

    let seed = u64::from(trng.random()) << 32 | u64::from(trng.random());
    static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        interfaces.access_point,
        embassy_net::Config::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(PORTAL_IP, 24),
            gateway: Some(PORTAL_IP),
            dns_servers: Default::default(),
        }),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(dhcp_task(stack).unwrap());
    spawner.spawn(dns_task(stack).unwrap());
    state::update(|s| s.link = Link::Setup(SetupStage::WaitingForClient));

    // Phones open several connections at once while probing for a captive
    // portal; a single-connection server makes them time out and give up.
    let portal = Portal {
        stack,
        networks,
        store: Mutex::new(store),
        pending: Mutex::new(None),
        controller: Mutex::new(Some(controller)),
    };
    join3(portal.serve(), portal.serve(), portal.serve()).await;
    unreachable!()
}

async fn scan(controller: &mut WifiController<'static>) -> Vec<String> {
    let mut found = match controller.scan_async(&Default::default()).await {
        Ok(found) => found,
        Err(e) => {
            warn!("WiFi scan failed: {e:?}");
            return Vec::new();
        }
    };
    found.sort_unstable_by_key(|ap| core::cmp::Reverse(ap.signal_strength));

    let mut names: Vec<String> = Vec::new();
    for ap in &found {
        let name = ap.ssid.as_str();
        if !name.is_empty() && !names.iter().any(|known| known == name) {
            names.push(name.into());
        }
    }
    names.truncate(12);
    debug!("Scan found {} networks", names.len());
    names
}

#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    let mut server = DhcpServer::<4, 1>::new(
        PORTAL_IP,
        Ipv4Addr::new(255, 255, 255, 0),
        PORTAL_IP,
        PORTAL_IP,
        Ipv4Addr::new(192, 168, 4, 50),
        Ipv4Addr::new(192, 168, 4, 60),
    );
    server
        .run_with_callback(stack, |event| {
            if let TransactionEvent::Leased(ip, _) = event {
                debug!("Setup client joined as {ip}");
                state::update(|s| {
                    if s.link == Link::Setup(SetupStage::WaitingForClient) {
                        s.link = Link::Setup(SetupStage::ClientJoined);
                    }
                });
            }
        })
        .await
}

/// Answers every A query with the portal's address. That is what makes a
/// phone's connectivity check land on the portal and open the sign-in sheet.
#[embassy_executor::task]
async fn dns_task(stack: Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buffer = vec![0u8; 1024];
    let mut tx_buffer = vec![0u8; 1024];
    let mut socket =
        UdpSocket::new(stack, &mut rx_meta, &mut rx_buffer, &mut tx_meta, &mut tx_buffer);
    socket.bind(53).expect("binding the DNS port");

    let mut query = [0u8; 512];
    let mut response = [0u8; 512 + 16];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut query).await else {
            continue;
        };
        if let Some(len) = dns_response(&query[..len], &mut response) {
            let _ = socket.send_to(&response[..len], from).await;
        }
    }
}

/// Build the response to a single-question DNS query; `None` for anything
/// that is not one.
fn dns_response(query: &[u8], response: &mut [u8]) -> Option<usize> {
    const HEADER: usize = 12;
    let is_query = query.get(2)? & 0x80 == 0;
    let questions = u16::from_be_bytes([*query.get(4)?, *query.get(5)?]);
    if !is_query || questions != 1 {
        return None;
    }

    // Walk the question's labels to find where it ends.
    let mut end = HEADER;
    loop {
        let label = usize::from(*query.get(end)?);
        if label & 0xC0 != 0 {
            return None;
        }
        end += 1 + label;
        if label == 0 {
            break;
        }
    }
    let qtype = u16::from_be_bytes([*query.get(end)?, *query.get(end + 1)?]);
    end += 4;
    query.get(end - 1)?;

    const TYPE_A: u16 = 1;
    let answer = qtype == TYPE_A;

    response[..end].copy_from_slice(&query[..end]);
    response[2] = 0x80 | (query[2] & 0x01); // response, recursion desired as asked
    response[3] = 0x80; // recursion available, no error
    response[6..8].copy_from_slice(&u16::from(answer).to_be_bytes());
    response[8..12].fill(0);
    if !answer {
        // AAAA and friends: the name exists, there is just nothing of that type.
        return Some(end);
    }

    let record = [
        0xC0, HEADER as u8, // the question's name
        0, 1, // A
        0, 1, // IN
        0, 0, 0, 10, // TTL: short, nothing here should outlive setup
        0, 4, // four address bytes
    ];
    response[end..end + record.len()].copy_from_slice(&record);
    end += record.len();
    response[end..end + 4].copy_from_slice(&PORTAL_IP.octets());
    Some(end + 4)
}

struct Portal {
    stack: Stack<'static>,
    networks: Vec<String>,
    store: Mutex<NoopRawMutex, Store>,
    /// Accepted settings, written once their confirmation page has been sent.
    pending: Mutex<NoopRawMutex, Option<Credentials>>,
    /// Owning this keeps WiFi up; dropping it takes the access point down.
    controller: Mutex<NoopRawMutex, Option<WifiController<'static>>>,
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    host: &'a str,
    body: &'a str,
}

impl Portal {
    async fn serve(&self) {
        let mut rx_buffer = vec![0u8; 2048];
        let mut tx_buffer = vec![0u8; 4096];
        let mut request = vec![0u8; 4096];
        loop {
            let mut socket = TcpSocket::new(self.stack, &mut rx_buffer, &mut tx_buffer);
            socket.set_timeout(Some(Duration::from_secs(10)));
            if socket.accept(80).await.is_err() {
                continue;
            }

            let restart = match with_timeout(Duration::from_secs(10), read_request(&mut socket, &mut request)).await {
                Ok(Some(len)) => match parse_request(&request[..len]) {
                    Some(request) => self.respond(&mut socket, &request).await,
                    None => false,
                },
                _ => false,
            };

            socket.close();
            let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
            socket.abort();

            if restart && let Some(credentials) = self.pending.lock().await.take() {
                // Time for the page to reach the phone and the screen to update.
                Timer::after(Duration::from_millis(1500)).await;
                // Take the access point down properly instead of just
                // vanishing in the reset: stopping WiFi disassociates the
                // phone, so it goes back to its own network right away rather
                // than hanging on to this one until it times out.
                drop(self.controller.lock().await.take());
                Timer::after(Duration::from_millis(300)).await;
                // No `.await` from here on; see `state::halt_ui_core`.
                if !state::halt_ui_core() {
                    warn!("UI core did not stop; writing anyway");
                }
                // `try_lock`, not `lock().await`: nothing else holds it, and
                // nothing may wait any more.
                let saved = self.store.try_lock().map(|mut store| store.save(&credentials));
                if !matches!(saved, Ok(Ok(()))) {
                    // Nothing was stored, so the restart lands in setup again.
                    warn!("Settings were not saved");
                }
                esp_hal::system::software_reset();
            }
        }
    }

    /// Returns whether a valid form was accepted into `pending`.
    async fn respond(&self, socket: &mut TcpSocket<'_>, request: &Request<'_>) -> bool {
        let for_us = request.host.split(':').next() == Some("192.168.4.1");
        match (request.method, request.path) {
            // Connectivity checks ask for other hosts. Redirecting them, rather
            // than answering, is what phones recognise as a captive portal.
            _ if !for_us => {
                send(socket, "302 Found", &format!("Location: {PORTAL_URL}\r\n"), "").await;
                false
            }
            ("POST", "/save") => match Credentials::from_form(request.body) {
                Ok(credentials) => {
                    info!("Setup form accepted for network '{}'", credentials.ssid);
                    state::update(|s| s.link = Link::Setup(SetupStage::Saving));
                    // Answer first. Once the UI core is stopped for the write
                    // there are no timers, and with them no reliable network.
                    send(socket, "200 OK", "", &page(&saved_page(&credentials.ssid))).await;
                    *self.pending.lock().await = Some(credentials);
                    true
                }
                Err(invalid) => {
                    info!("Setup form rejected: {invalid:?}");
                    // Only the network name is echoed back. The secrets stay
                    // out of the response even when the form has to be redone.
                    let ssid = form_field(request.body, "ssid");
                    let form = self.form(Some(invalid.message()), &ssid);
                    send(socket, "400 Bad Request", "", &page(&form)).await;
                    false
                }
            },
            ("GET", "/") => {
                state::update(|s| {
                    if matches!(s.link, Link::Setup(SetupStage::WaitingForClient | SetupStage::ClientJoined)) {
                        s.link = Link::Setup(SetupStage::FormOpened);
                    }
                });
                send(socket, "200 OK", "", &page(&self.form(None, ""))).await;
                false
            }
            _ => {
                send(socket, "302 Found", &format!("Location: {PORTAL_URL}\r\n"), "").await;
                false
            }
        }
    }

    fn form(&self, error: Option<&str>, ssid: &str) -> String {
        let error = error
            .map(|message| format!(r#"<p class="error">{}</p>"#, html_escape(message)))
            .unwrap_or_default();
        let options: String = self
            .networks
            .iter()
            .map(|name| format!(r#"<option value="{}">"#, html_escape(name)))
            .collect();
        format!(
            r#"<h1>Claude Monitor</h1>
<p>Connect the display to your WiFi and give it a token to read your usage with.</p>
{error}
<form method="post" action="/save" autocomplete="off">
<label for="ssid">WiFi network</label>
<input id="ssid" name="ssid" list="networks" value="{ssid}" required maxlength="32"
 autocapitalize="none" autocorrect="off" spellcheck="false">
<datalist id="networks">{options}</datalist>
<p class="hint">2.4 GHz, WPA2. The display has no 5 GHz radio.</p>
<label for="password">WiFi password</label>
<input id="password" name="password" type="password" maxlength="63">
<label for="token">Claude token</label>
<textarea id="token" name="token" rows="4" required
 autocapitalize="none" autocorrect="off" spellcheck="false" placeholder="sk-ant-oat01-..."></textarea>
<p class="hint">Run <code>claude setup-token</code> on a computer where Claude Code is
logged in, and paste what it prints.</p>
<button type="submit">Save and restart</button>
</form>"#,
            ssid = html_escape(ssid),
        )
    }
}

fn saved_page(ssid: &str) -> String {
    format!(
        "<h1>Got it</h1><p>The display is saving this and restarting to join <b>{}</b>. \
         This setup network is going away; your phone will return to its usual WiFi.</p>\
         <p>If it shows the setup code again, saving failed: repeat these steps. If it cannot \
         connect, hold a finger on its screen for three seconds to start over.</p>",
        html_escape(ssid)
    )
}

fn page(content: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Claude Monitor setup</title><style>
body{{font:16px/1.45 system-ui,sans-serif;background:#141413;color:#faf9f5;margin:0;padding:24px 20px;max-width:30em}}
h1{{color:#d97757;font-size:1.4em;margin:0 0 .4em}}
label{{display:block;margin:1.2em 0 .3em;font-weight:600}}
input,textarea{{width:100%;box-sizing:border-box;font:inherit;padding:.6em;border-radius:8px;border:1px solid #30302e;background:#1f1e1d;color:inherit}}
textarea{{font-family:ui-monospace,monospace;font-size:.85em;word-break:break-all}}
button{{margin-top:1.6em;width:100%;font:inherit;font-weight:600;padding:.8em;border:0;border-radius:8px;background:#d97757;color:#141413}}
.hint{{color:#9c9a92;font-size:.85em;margin:.4em 0 0}}
.error{{background:#e5534b22;border:1px solid #e5534b;border-radius:8px;padding:.6em .8em}}
code{{color:#d97757}}
</style></head><body>{content}</body></html>"#
    )
}

/// Pull one decoded field back out of a form body, for re-filling the form.
fn form_field(body: &str, name: &str) -> String {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(field, _)| *field == name)
        .map(|(_, value)| url_decode(value).trim().into())
        .unwrap_or_default()
}

/// Read a request head, plus the body its Content-Length announces. Returns
/// the total length, or `None` if it does not fit or the peer went away.
async fn read_request(socket: &mut TcpSocket<'_>, buffer: &mut [u8]) -> Option<usize> {
    let mut filled = 0;
    let mut expected = None;
    loop {
        if expected.is_none()
            && let Some(head_end) = buffer[..filled].windows(4).position(|w| w == b"\r\n\r\n")
        {
            let head = core::str::from_utf8(&buffer[..head_end]).ok()?;
            let body_len = header(head, "content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            expected = Some(head_end + 4 + body_len);
        }
        match expected {
            Some(total) if total > buffer.len() => return None,
            Some(total) if filled >= total => return Some(total),
            _ => {}
        }
        if filled == buffer.len() {
            return None;
        }
        match socket.read(&mut buffer[filled..]).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => filled += n,
        }
    }
}

fn parse_request(raw: &[u8]) -> Option<Request<'_>> {
    let raw = core::str::from_utf8(raw).ok()?;
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let mut request_line = head.lines().next()?.split(' ');
    let method = request_line.next()?;
    let target = request_line.next()?;
    let path = target.split('?').next()?;
    Some(Request { method, path, host: header(head, "host").unwrap_or(""), body })
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field.trim().eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

async fn send(socket: &mut TcpSocket<'_>, status: &str, extra_headers: &str, body: &str) {
    // `no-store`: the captive-portal sheet must not cache the redirect or a
    // stale form, and nothing about this page should persist on the phone.
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         {extra_headers}\r\n",
        body.len()
    );
    for part in [head.as_bytes(), body.as_bytes()] {
        let mut remaining = part;
        while !remaining.is_empty() {
            match socket.write(remaining).await {
                Ok(0) | Err(_) => return,
                Ok(n) => remaining = &remaining[n..],
            }
        }
    }
}
