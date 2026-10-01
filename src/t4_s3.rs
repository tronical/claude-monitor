//! Board support for the LilyGO T4-S3: a 2.41" 600×450 RM690B0 AMOLED on
//! QSPI, a CST226SE touch panel on I²C and an SY6970 charger on the same bus.
//!
//! It stands in for Slint's `mcu-board-support`, which has no T4-S3 target,
//! and keeps the same shape: [`init`] sets up the hardware, the PSRAM heap and
//! the Slint platform, and the event loop busy-polls touch and never yields.
//!
//! The panel only accepts update windows that start on an even column and row
//! and end on an odd one. Slint renders into a full frame buffer in PSRAM, and
//! each dirty rectangle is widened to that grid before it is sent.
//!
//! Pins, the init sequence and the touch report format follow LilyGO's own
//! driver (`LilyGo-AMOLED-Series`, `BOARD_AMOLED_241`).

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::RefCell;

use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::spi::Mode as SpiMode;
use esp_hal::spi::master::{Address, Command, Config as SpiConfig, DataMode, Spi, SpiDmaBus};
use esp_hal::time::{Instant, Rate};
use log::{info, warn};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
use slint::platform::{PointerEventButton, WindowEvent};

pub use esp_hal::main as entry;

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("Panic: {:?}", info);
    loop {}
}

/// The panel in landscape, which is how the UI is laid out.
const WIDTH: usize = 600;
const HEIGHT: usize = 450;

/// The UI is designed at 320×240; the panel has the same 4:3 shape. The build
/// script bakes the same factor into the compiled UI so that glyphs are
/// rasterised at the size they are shown.
pub const SCALE_FACTOR: f32 = 1.875;
const _: () = assert!(WIDTH as f32 == 320.0 * SCALE_FACTOR && HEIGHT as f32 == 240.0 * SCALE_FACTOR);

/// Bytes per DMA transfer of pixel data: a few rows of the panel. The DMA
/// buffer has to be in internal RAM, which the radio needs too.
const DMA_TX_BYTES: usize = 8 * 1024;

// RM690B0 commands (MIPI DCS) and the QSPI framing around them.
const QSPI_WRITE_COMMAND: u16 = 0x02;
const QSPI_WRITE_PIXELS: u16 = 0x32;
const CASET: u8 = 0x2A;
const RASET: u8 = 0x2B;
const RAMWR: u8 = 0x2C;
const MADCTL: u8 = 0x36;
const BRIGHTNESS: u8 = 0x51;
/// Row and column exchange plus column mirroring: landscape, the way LilyGO's
/// driver sets up rotation 0.
const MADCTL_LANDSCAPE: u8 = 0x40 | 0x20;
/// In this orientation the visible area starts 16 rows into the panel's RAM.
const ROW_OFFSET: u16 = 16;
/// LilyGO's default; the init sequence leaves the panel at full brightness.
const DEFAULT_BRIGHTNESS: u8 = 175;

/// `(command, parameters, delay in ms after it)`, from LilyGO's `rm690b0_cmd`.
const INIT_SEQUENCE: &[(u8, &[u8], u32)] = &[
    (0xFE, &[0x20], 0), // page 0x20
    (0x26, &[0x0A], 0), // MIPI off
    (0x24, &[0x80], 0), // SPI writes RAM
    (0x5A, &[0x51], 0), // SWIRE for the BV6804 supply
    (0x5B, &[0x2E], 0),
    (0xFE, &[0x00], 0),  // page 0
    (0x3A, &[0x55], 0),  // 16 bits per pixel
    (0xC2, &[0x00], 10), //
    (0x35, &[0x00], 0),  // tearing effect line on
    (BRIGHTNESS, &[0x00], 0),
    (0x11, &[], 120), // sleep out
    (0x29, &[], 10),  // display on
];

const CST226SE_ADDRESS: u8 = 0x5A;
const SY6970_ADDRESS: u8 = 0x6A;

type BoardI2c = I2c<'static, esp_hal::Blocking>;

struct Panel {
    spi: SpiDmaBus<'static, esp_hal::Blocking>,
    /// Driven by hand so that one write can span several DMA transfers.
    cs: Output<'static>,
    /// Pixels converted to the panel's byte order, one transfer's worth.
    chunk: Box<[u8]>,
}

impl Panel {
    fn command(&mut self, command: u8, parameters: &[u8]) {
        self.cs.set_low();
        let result = self.spi.half_duplex_write(
            DataMode::Single,
            Command::_8Bit(QSPI_WRITE_COMMAND, DataMode::Single),
            Address::_24Bit(u32::from(command) << 8, DataMode::Single),
            0,
            parameters,
        );
        self.cs.set_high();
        if let Err(error) = result {
            warn!("Display command {command:#04x} failed: {error:?}");
        }
    }

    fn init(&mut self) {
        let delay = Delay::new();
        // LilyGO's driver sends the sequence twice: the first pass does not
        // always take straight after power-up.
        for _ in 0..2 {
            for &(command, parameters, wait_ms) in INIT_SEQUENCE {
                self.command(command, parameters);
                if wait_ms > 0 {
                    delay.delay_millis(wait_ms);
                }
            }
        }
        self.command(MADCTL, &[MADCTL_LANDSCAPE]);
        self.command(BRIGHTNESS, &[DEFAULT_BRIGHTNESS]);
    }

    /// Send the inclusive rectangle `x0..=x1` × `y0..=y1` from `frame`, which
    /// must already be aligned to the panel's 2×2 grid.
    fn flush(&mut self, frame: &[Rgb565Pixel], (x0, y0): (usize, usize), (x1, y1): (usize, usize)) {
        let [xs, xe] = [x0 as u16, x1 as u16].map(u16::to_be_bytes);
        let [ys, ye] = [y0 as u16 + ROW_OFFSET, y1 as u16 + ROW_OFFSET].map(u16::to_be_bytes);
        self.command(CASET, &[xs[0], xs[1], xe[0], xe[1]]);
        self.command(RASET, &[ys[0], ys[1], ye[0], ye[1]]);

        // One write: the command and address go out once, then the pixels
        // follow in as many transfers as it takes while CS stays low.
        let Self { spi, cs, chunk } = self;
        let mut filled = 0;
        let mut first = true;
        cs.set_low();
        for y in y0..=y1 {
            for pixel in &frame[y * WIDTH + x0..=y * WIDTH + x1] {
                // The panel takes RGB565 most significant byte first.
                chunk[filled..filled + 2].copy_from_slice(&pixel.0.to_be_bytes());
                filled += 2;
                if filled == chunk.len() {
                    send_pixels(spi, &chunk[..], &mut first);
                    filled = 0;
                }
            }
        }
        if filled > 0 {
            send_pixels(spi, &chunk[..filled], &mut first);
        }
        cs.set_high();
    }
}

/// One transfer of pixel data. The first of a write carries the RAMWR
/// command; the rest continue it.
fn send_pixels(spi: &mut SpiDmaBus<'static, esp_hal::Blocking>, bytes: &[u8], first: &mut bool) {
    let (command, address) = if core::mem::take(first) {
        (
            Command::_8Bit(QSPI_WRITE_PIXELS, DataMode::Single),
            Address::_24Bit(u32::from(RAMWR) << 8, DataMode::Single),
        )
    } else {
        (Command::None, Address::None)
    };
    if let Err(error) = spi.half_duplex_write(DataMode::Quad, command, address, 0, bytes) {
        warn!("Display write failed: {error:?}");
    }
}

/// CST226SE, read the way LilyGO's SensorLib does it.
struct Touch {
    i2c: BoardI2c,
}

impl Touch {
    /// The first touch point in panel coordinates, if a finger is down.
    /// `Err` is a failed read, which says nothing about the finger.
    fn read(&mut self) -> Result<Option<(i32, i32)>, esp_hal::i2c::master::Error> {
        let mut report = [0u8; 28];
        self.i2c.write_read(CST226SE_ADDRESS, &[0x00], &mut report)?;

        // Not a touch report: a key event, or nothing new.
        if report[6] != 0xAB || report[0] == 0xAB || report[5] == 0x80 {
            return Ok(None);
        }
        let points = report[5] & 0x7F;
        if points == 0 || points > 5 {
            // Acknowledge, so that the controller reports again.
            self.i2c.write(CST226SE_ADDRESS, &[0x00, 0xAB])?;
            return Ok(None);
        }

        let raw_x = (i32::from(report[1]) << 4) | i32::from(report[3] >> 4);
        let raw_y = (i32::from(report[2]) << 4) | i32::from(report[3] & 0x0F);
        // The controller reports in portrait; the panel is turned to
        // landscape. Matches the touch mapping LilyGO uses for rotation 0.
        let x = raw_y.clamp(0, WIDTH as i32 - 1);
        let y = (HEIGHT as i32 - raw_x).clamp(0, HEIGHT as i32 - 1);
        Ok(Some((x, y)))
    }
}

/// Board hardware constructed in [`init`] and consumed by the event loop.
struct BoardState {
    panel: Panel,
    touch: Touch,
    frame: Box<[Rgb565Pixel]>,
    // Held high for as long as the board runs.
    _power: Output<'static>,
    _display_reset: Output<'static>,
    _touch_reset: Output<'static>,
}

struct Backend {
    window: RefCell<Option<Rc<MinimalSoftwareWindow>>>,
    state: RefCell<Option<BoardState>>,
}

impl slint::platform::Platform for Backend {
    fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        self.window.replace(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_millis(Instant::now().duration_since_epoch().as_millis())
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        self.run_event_loop()
    }
}

/// Initializes the heap, the board peripherals and sets the Slint platform.
pub fn init() {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz));
    esp_println::logger::init_logger_from_env();

    // The ESP32-S3R8 has 8 MB of octal PSRAM. Registered before anything
    // allocates so that it is the heap's first region.
    esp_alloc::psram_allocator!(
        peripherals.PSRAM,
        esp_hal::psram,
        esp_hal::psram::PsramConfig { mode: esp_hal::psram::PsramMode::OctalSpi, ..Default::default() }
    );

    let delay = Delay::new();

    // Powers the panel (and the rest of the 3.3 V peripherals).
    let power = Output::new(peripherals.GPIO9, Level::High, OutputConfig::default());

    let mut display_reset = Output::new(peripherals.GPIO13, Level::High, OutputConfig::default());
    delay.delay_millis(200);
    display_reset.set_low();
    delay.delay_millis(300);
    display_reset.set_high();
    delay.delay_millis(200);

    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(32, DMA_TX_BYTES);
    let spi = Spi::new(
        peripherals.SPI2,
        // LilyGO runs the panel at 36 MHz.
        SpiConfig::default().with_frequency(Rate::from_mhz(36)).with_mode(SpiMode::_0),
    )
    .expect("display SPI configuration")
    .with_sck(peripherals.GPIO15)
    .with_sio0(peripherals.GPIO14)
    .with_sio1(peripherals.GPIO10)
    .with_sio2(peripherals.GPIO16)
    .with_sio3(peripherals.GPIO12)
    .with_dma(peripherals.DMA_CH0)
    .with_buffers(
        DmaRxBuf::new(rx_descriptors, rx_buffer).expect("DMA receive buffer"),
        DmaTxBuf::new(tx_descriptors, tx_buffer).expect("DMA transmit buffer"),
    );
    let cs = Output::new(peripherals.GPIO11, Level::High, OutputConfig::default());
    let mut panel = Panel { spi, cs, chunk: alloc::vec![0; DMA_TX_BYTES].into_boxed_slice() };
    panel.init();
    info!("Display initialized");

    let mut touch_reset = Output::new(peripherals.GPIO17, Level::Low, OutputConfig::default());
    delay.delay_millis(100);
    touch_reset.set_high();
    delay.delay_millis(100);

    let mut i2c = I2c::new(peripherals.I2C0, I2cConfig::default().with_frequency(Rate::from_khz(400)))
        .expect("I2C configuration")
        .with_sda(peripherals.GPIO6)
        .with_scl(peripherals.GPIO7);

    // Without a battery the charger's status LED flickers; it means nothing
    // on a desk display, so switch it off (REG07 bit 6, STAT_DIS).
    let mut reg07 = [0u8];
    match i2c.write_read(SY6970_ADDRESS, &[0x07], &mut reg07) {
        Ok(()) => {
            if let Err(error) = i2c.write(SY6970_ADDRESS, &[0x07, reg07[0] | 0x40]) {
                warn!("Could not switch off the charge LED: {error:?}");
            }
        }
        Err(error) => warn!("Charger not found: {error:?}"),
    }

    let frame = alloc::vec![Rgb565Pixel(0); WIDTH * HEIGHT].into_boxed_slice();

    slint::platform::set_platform(Box::new(Backend {
        window: RefCell::new(None),
        state: RefCell::new(Some(BoardState {
            panel,
            touch: Touch { i2c },
            frame,
            _power: power,
            _display_reset: display_reset,
            _touch_reset: touch_reset,
        })),
    }))
    .expect("backend already initialized");
}

impl Backend {
    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        let BoardState { mut panel, mut touch, mut frame, .. } =
            self.state.borrow_mut().take().expect("event loop already running");

        self.window
            .borrow()
            .as_ref()
            .expect("window created before the event loop runs")
            .set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));

        let mut last_touch = None;

        loop {
            slint::platform::update_timers_and_animations();

            let Some(window) = self.window.borrow().clone() else {
                continue;
            };

            match touch.read() {
                Ok(Some((x, y))) => {
                    let pos = slint::PhysicalPosition::new(x, y).to_logical(window.scale_factor());
                    let event = if let Some(previous) = last_touch.replace(pos) {
                        // A still finger is not an event, but the frame must
                        // still be drawn below: timers keep changing the UI
                        // while it is held down.
                        (previous != pos).then_some(WindowEvent::PointerMoved { position: pos })
                    } else {
                        Some(WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left })
                    };
                    if let Some(event) = event {
                        window.dispatch_event_with_result(event)?;
                    }
                }
                Ok(None) => {
                    if let Some(pos) = last_touch.take() {
                        window.dispatch_event_with_result(WindowEvent::PointerReleased {
                            position: pos,
                            button: PointerEventButton::Left,
                        })?;
                        window.dispatch_event_with_result(WindowEvent::PointerExited)?;
                    }
                }
                // A failed read says nothing about the finger; try again.
                Err(_) => {}
            }

            window.draw_if_needed(|renderer| {
                let dirty = renderer.render(&mut frame, WIDTH);
                for (origin, size) in dirty.iter() {
                    if size.width == 0 || size.height == 0 {
                        continue;
                    }
                    // Widen to the panel's 2×2 grid: start even, end odd.
                    let x0 = origin.x.max(0) as usize & !1;
                    let y0 = origin.y.max(0) as usize & !1;
                    let x1 = ((origin.x as usize + size.width as usize - 1) | 1).min(WIDTH - 1);
                    let y1 = ((origin.y as usize + size.height as usize - 1) | 1).min(HEIGHT - 1);
                    panel.flush(&frame, (x0, y0), (x1, y1));
                }
            });
        }
    }
}
