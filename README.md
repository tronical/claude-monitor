# claude-monitor

A desk display for Claude subscription usage on an ESP32-S3-BOX-3: five-hour
and seven-day utilisation, reset countdowns, and whether the current pace runs
into the limit before the window resets.

It is standalone. There is no companion program on a computer: the box joins
WiFi and asks the Anthropic API itself, once a minute. Tap the screen to poll
immediately.

Bare-metal Rust (`no_std`, esp-hal), no ESP-IDF. UI in [Slint](https://slint.dev).

## Setup

You need the `esp` Rust toolchain ([espup](https://github.com/esp-rs/espup))
and `espflash`.

```sh
. ~/export-esp.sh
cargo run --release        # builds, flashes, and opens the serial monitor
```

A freshly flashed box starts in setup mode and is configured from a phone:

1. The screen shows a QR code. Scan it to join the box's own WiFi network
   (the name and password are on screen too, and change on every boot).
2. The phone's sign-in sheet opens with the setup form. If it does not, the
   screen has moved on to a second QR code for `http://192.168.4.1/`.
3. Pick your WiFi network, enter its password, and paste a token from
   `claude setup-token`. The token is on your computer, not your phone:
   Universal Clipboard carries it across on Apple devices, or join the setup
   network from the computer instead and fill the form in there.
4. The box saves the settings to flash, takes its network down, restarts and
   joins yours.

Your WiFi must be 2.4 GHz and WPA2; the bare-metal driver does not do
WPA3-only networks.

To change the settings later, hold a finger on the screen for three seconds and
confirm. That is also the way out when the box cannot join the network any
more; it never drops into setup mode by itself, so a rebooting router cannot
strand it there.

For development, `secrets.env` (see `secrets.env.example`) bakes settings into
the firmware so that a wiped box comes up configured. Settings saved through
the form take precedence over it.

### Where the secrets live

The setup form travels over plain HTTP, but inside a WPA2 network whose
password is random per boot and shown only on the device's own screen, so
reading it takes being in the room. Afterwards the WiFi password and the token
sit unencrypted in flash, where anyone holding the board can read them out.
Treat the box like a logged-in laptop, and revoke the token if it goes missing.

### Why `claude setup-token`

Claude Code's own credential in `~/.claude/.credentials.json` rotates regularly,
so a device holding a copy of it is soon locked out. `claude setup-token` issues
a long-lived (one year) subscription token instead, which the device can keep.

## How it works

There is no usage endpoint for subscriptions. The numbers ride on the
`anthropic-ratelimit-unified-*` response headers of any `/v1/messages` call, so
the device sends the cheapest request it can (Haiku, `max_tokens: 1`), reads the
response head, and drops the connection. Each poll therefore costs a few tokens
of subscription usage.

The device has no clock. Reset times arrive as absolute epoch stamps; the
server's own `Date` header is the reference that turns them into countdowns,
which then run on the monotonic timer between polls.

| Core | Job |
|---|---|
| 0 | Slint event loop from `mcu-board-support` (display, touch). It busy-polls and never yields. |
| 1 | embassy executor: esp-radio WiFi, embassy-net, mbedtls TLS, the poll loop. |

Slint is built single-threaded, so the cores share only a small `Copy` snapshot
(`src/state.rs`) that a Slint timer polls twice a second.

Flash writes need care with this split. Writing stalls the other core wherever
it is; stalled inside a critical section it keeps the global lock and the
writer deadlocks. And the embassy timer interrupt is serviced by core 0, so
once that core stops, no timer on core 1 fires again. Every write here is
followed by a restart, which makes the answer simple: core 0 halts itself from
its timer callback, holding nothing, and core 1 then writes and resets without
waiting on anything (`state::halt_ui_core`).

TLS is mbedtls (via `mbedtls-rs`) with chain and hostname verification against
the roots in `certs/`; see `certs/README.md` for the one thing it cannot check.

## Layout

- `ui/main.slint` — the 320×240 UI. Preview with `slint-viewer ui/main.slint`.
- `src/usage.rs` — response header parsing; `core`-only and host-testable
  (the command is at the top of the file).
- `src/net.rs` — WiFi, IP stack, HTTPS probe (core 1).
- `src/setup.rs` — setup mode: access point, DHCP, catch-all DNS, the form.
- `src/config.rs` — the settings record and form decoding; host-testable like
  `usage.rs`.
- `src/storage.rs` — the settings record in the `nvs` flash partition.
- `src/main.rs` — startup, core split, and turning snapshots into UI text.
- `tools/monitor.py` — resets the board and prints its log. `espflash monitor`
  cannot attach to the already-running app on this board; this can:
  `tools/monitor.py /dev/cu.usbmodem1101 30`. Add `noreset` to attach without
  restarting it, which matters in setup mode where a restart changes the
  access point's password.

## Version pins

The esp stack is held on the esp-hal 1.1 line (esp-radio 0.18, esp-rtos 0.3)
because that is what Slint's board support pins. Move everything together when
Slint moves to esp-hal 1.2; at that point `mbedtls-rs`'s `esp32s3` feature can
also be enabled for hardware-accelerated crypto.
