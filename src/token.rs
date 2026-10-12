//! Replacing the Claude token from a phone on the same WiFi, without going
//! back through setup.
//!
//! Asked for from the hold menu, the display serves a one-field form on its
//! own address, under a random path that only its screen shows, as a QR code.
//! The page closes again on "cancel", after a while, or once a new token is
//! in; that one is saved and the device restarts with it.
//!
//! Unlike setup, the form crosses the home network in plain HTTP: whoever can
//! read that traffic (on WPA2-Personal, anyone with its password) can read the
//! token as it is submitted. The random path only keeps others on the network
//! from replacing it.

use alloc::format;
use alloc::string::String;
use alloc::vec;

use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::rng::Trng;
use log::info;

use crate::config::{Settings, html_escape};
use crate::setup::{Request, page, parse_request, random_char, read_request, send};
use crate::state::{self, TOKEN_KEY_LEN, TokenPage};

/// Long enough to go and run `claude setup-token` somewhere.
const OPEN_FOR: Duration = Duration::from_secs(10 * 60);

/// Settings with a new token, for `net::reconfigure_task` to write: flash is
/// only ever written from there.
pub static SAVE: Signal<CriticalSectionRawMutex, Settings> = Signal::new();

#[embassy_executor::task]
pub async fn task(stack: Stack<'static>, settings: &'static Settings) {
    let mut trng = Trng::try_new().expect("TRNG source is alive");
    loop {
        // A "close" with nothing open is stale; only an "open" starts one.
        while !state::TOKEN_PAGE.wait().await {}
        let key: [u8; TOKEN_KEY_LEN] = core::array::from_fn(|_| random_char(&mut trng));

        let closed = async { while state::TOKEN_PAGE.wait().await {} };
        match select3(open(stack, settings, &key), closed, Timer::after(OPEN_FOR)).await {
            Either3::First(new) => {
                state::update(|s| s.token_page = Some(TokenPage::Saving));
                // Time for the screen to say so before the UI core stops.
                Timer::after(Duration::from_millis(500)).await;
                SAVE.signal(new);
                return;
            }
            _ => {
                info!("Token page closed");
                state::update(|s| s.token_page = None);
            }
        }
    }
}

/// Serve the page until a valid token comes in.
async fn open(
    stack: Stack<'static>,
    settings: &'static Settings,
    key: &[u8; TOKEN_KEY_LEN],
) -> Settings {
    state::update(|s| s.token_page = Some(TokenPage::WaitingForAddress));
    stack.wait_config_up().await;
    let address = match stack.config_v4() {
        Some(config) => config.address.address(),
        // Gone again already; it comes back or the page times out.
        None => core::future::pending().await,
    };
    state::update(|s| s.token_page = Some(TokenPage::Open { address, key: *key }));
    info!("Token page open at http://{address}/");

    let path = format!("/{}", core::str::from_utf8(key).expect("key is ASCII"));
    // Two at a time: browsers open a spare connection alongside the real one.
    match select(serve(stack, settings, &path), serve(stack, settings, &path)).await {
        Either::First(new) | Either::Second(new) => new,
    }
}

async fn serve(stack: Stack<'static>, settings: &Settings, path: &str) -> Settings {
    let mut rx_buffer = vec![0u8; 2048];
    let mut tx_buffer = vec![0u8; 4096];
    let mut request = vec![0u8; 4096];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(80).await.is_err() {
            continue;
        }

        let accepted =
            match with_timeout(Duration::from_secs(10), read_request(&mut socket, &mut request))
                .await
            {
                Ok(Some(len)) => match parse_request(&request[..len]) {
                    Some(request) => respond(&mut socket, &request, settings, path).await,
                    None => None,
                },
                _ => None,
            };

        socket.close();
        let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
        socket.abort();

        if let Some(new) = accepted {
            return new;
        }
    }
}

/// Returns the new settings once a valid token has been accepted.
async fn respond(
    socket: &mut TcpSocket<'_>,
    request: &Request<'_>,
    settings: &Settings,
    path: &str,
) -> Option<Settings> {
    if request.path != path {
        // Says nothing about what is here, or what the right path would be.
        send(socket, "404 Not Found", "", &page("<h1>Not found</h1>")).await;
        return None;
    }
    match request.method {
        "POST" => match settings.with_token_from_form(request.body) {
            Ok(new) => {
                info!("New token accepted");
                send(socket, "200 OK", "", &page(DONE)).await;
                Some(new)
            }
            Err(invalid) => {
                info!("Token form rejected: {invalid:?}");
                send(socket, "400 Bad Request", "", &page(&form(path, Some(invalid.message()))))
                    .await;
                None
            }
        },
        _ => {
            send(socket, "200 OK", "", &page(&form(path, None))).await;
            None
        }
    }
}

fn form(path: &str, error: Option<&str>) -> String {
    let error = error
        .map(|message| format!(r#"<p class="error">{}</p>"#, html_escape(message)))
        .unwrap_or_default();
    format!(
        r#"<h1>Claude token</h1>
<p>Give the display a new token. Its WiFi networks stay as they are.</p>
{error}
<form method="post" action="{path}" autocomplete="off">
<label for="token">Claude token</label>
<textarea id="token" name="token" rows="4" required
 autocapitalize="none" autocorrect="off" spellcheck="false" placeholder="sk-ant-oat01-..."></textarea>
<p class="hint">Run <code>claude setup-token</code> on a computer where Claude Code is logged in, and paste what it prints.</p>
<button type="submit">Save and restart</button>
</form>"#
    )
}

const DONE: &str = "<h1>Got it</h1><p>The display is saving the new token and restarting; \
     it shows your usage again in a few seconds.</p><p>If it says the token was rejected, \
     hold a finger on its screen for three seconds to try another.</p>";
