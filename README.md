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
cp secrets.env.example secrets.env
$EDITOR secrets.env        # WiFi credentials and a token from `claude setup-token`
. ~/export-esp.sh
cargo run --release        # builds, flashes, and opens the serial monitor
```

`secrets.env` is git-ignored. Its values are compiled into the firmware, so
anyone with the board can read them out of flash; treat the box like a logged-in
laptop. Revoke the token from your Claude account settings if the box goes missing.

### Why `claude setup-token`

Claude Code's own credential in `~/.claude/.credentials.json` rotates every few
hours, so a device holding a copy of it is soon locked out. `claude setup-token`
issues a long-lived (one year) subscription token instead, which the device can
keep.

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

TLS is mbedtls (via `mbedtls-rs`) with chain and hostname verification against
the roots in `certs/`; see `certs/README.md` for the one thing it cannot check.

## Layout

- `ui/main.slint` — the 320×240 UI. Preview with `slint-viewer ui/main.slint`.
- `src/usage.rs` — response header parsing; `core`-only and host-testable
  (the command is at the top of the file).
- `src/net.rs` — WiFi, IP stack, HTTPS probe (core 1).
- `src/main.rs` — startup, core split, and turning snapshots into UI text.
- `tools/monitor.py` — resets the board and prints its log. `espflash monitor`
  cannot attach to the already-running app on this board; this can:
  `tools/monitor.py /dev/cu.usbmodem1101 30`

## Version pins

The esp stack is held on the esp-hal 1.1 line (esp-radio 0.18, esp-rtos 0.3)
because that is what Slint's board support pins. Move everything together when
Slint moves to esp-hal 1.2; at that point `mbedtls-rs`'s `esp32s3` feature can
also be enabled for hardware-accelerated crypto.
