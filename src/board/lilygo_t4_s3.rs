//! Board support for the LilyGO T4-S3, which Slint's `mcu-board-support` does
//! not cover: a 2.41" 600x450 RM690B0 AMOLED on quad SPI and a CST226SE touch
//! controller on I2C. Pins, the panel's init sequence and the touch report
//! format follow LilyGO's own LilyGo-AMOLED-Series and SensorLib.
//!
//! Shaped like the BOX-3's board support: `init` builds the hardware and sets
//! the Slint platform, whose event loop busy-polls touch and never yields.
//! The difference is the frame buffer. The panel only takes address windows
//! that start and end on even coordinates, which a renderer working line by
//! line cannot give it, so Slint renders the whole frame into PSRAM and the
//! dirty rectangles, widened to even bounds, are copied out to the panel.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use core::cell::RefCell;
use core::ops::Range;

use esp_backtrace as _;
use esp_hal::Blocking;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Address, Command, Config as SpiConfig, DataMode, Spi, SpiDmaBus};
use esp_hal::time::{Instant, Rate};
use log::{info, warn};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{PhysicalPosition, PhysicalSize};

pub use esp_hal::main as entry;

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("Panic: {:?}", info);
    loop {}
}

const WIDTH: usize = 600;
const HEIGHT: usize = 450;

/// Bytes per quad-SPI transfer; also the size of the DMA buffer.
const CHUNK: usize = 8 * 1024;

/// Initializes the heap, the board peripherals and sets the Slint platform.
pub fn init() {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz));
    esp_println::logger::init_logger_from_env();

    // Register the PSRAM heap before anything allocates, as on the BOX-3.
    esp_alloc::psram_allocator!(
        peripherals.PSRAM,
        esp_hal::psram,
        esp_hal::psram::PsramConfig {
            mode: esp_hal::psram::PsramMode::OctalSpi,
            ..Default::default()
        }
    );

    let delay = Delay::new();

    // The panel's supply is switched by GPIO9.
    let panel_power = Output::new(peripherals.GPIO9, Level::High, OutputConfig::default());
    let mut panel_reset = Output::new(peripherals.GPIO13, Level::High, OutputConfig::default());
    delay.delay_millis(200);
    panel_reset.set_low();
    delay.delay_millis(300);
    panel_reset.set_high();
    delay.delay_millis(200);

    // Chip select is a plain GPIO: a run of pixels spans several transfers
    // and the panel must see it as one.
    let cs = Output::new(peripherals.GPIO11, Level::High, OutputConfig::default());
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(4, CHUNK);
    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default().with_frequency(Rate::from_mhz(40)).with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO15)
    .with_sio0(peripherals.GPIO14)
    .with_sio1(peripherals.GPIO10)
    .with_sio2(peripherals.GPIO16)
    .with_sio3(peripherals.GPIO12)
    .with_dma(peripherals.DMA_CH0)
    .with_buffers(
        DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap(),
        DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap(),
    );

    let mut panel = Panel { spi, cs, staging: Box::new([0; CHUNK]) };
    panel.init(&delay);

    // Touch shares its I2C bus with the charger, which is left at its
    // defaults.
    let mut touch_reset = Output::new(peripherals.GPIO17, Level::Low, OutputConfig::default());
    delay.delay_millis(100);
    touch_reset.set_high();
    delay.delay_millis(100);
    let i2c = I2c::new(peripherals.I2C0, I2cConfig::default().with_frequency(Rate::from_khz(400)))
        .unwrap()
        .with_sda(peripherals.GPIO6)
        .with_scl(peripherals.GPIO7);
    let touch = Touch { i2c };

    // Black rather than whatever the panel's memory held at power-up, until
    // the first frame lands.
    let frame = vec![Rgb565Pixel(0); WIDTH * HEIGHT].into_boxed_slice();
    panel.flush(&frame, 0..WIDTH, 0..HEIGHT);
    info!("Board initialized");

    slint::platform::set_platform(Box::new(Backend {
        window: RefCell::new(None),
        state: RefCell::new(Some(BoardState {
            panel,
            touch,
            frame,
            _panel_power: panel_power,
            _panel_reset: panel_reset,
            _touch_reset: touch_reset,
        })),
    }))
    .expect("backend already initialized");
}

/// Board hardware constructed in [`init`] and consumed by the event loop.
struct BoardState {
    panel: Panel,
    touch: Touch,
    frame: Box<[Rgb565Pixel]>,
    // Kept alive, and so driven, for the lifetime of the event loop.
    _panel_power: Output<'static>,
    _panel_reset: Output<'static>,
    _touch_reset: Output<'static>,
}

struct Backend {
    window: RefCell<Option<Rc<MinimalSoftwareWindow>>>,
    state: RefCell<Option<BoardState>>,
}

impl slint::platform::Platform for Backend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        self.window.replace(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_millis(Instant::now().duration_since_epoch().as_millis())
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        let BoardState { mut panel, mut touch, mut frame, .. } =
            self.state.borrow_mut().take().expect("event loop already running");
        let window = self.window.borrow().clone().expect("a window was created");
        window.set_size(PhysicalSize::new(WIDTH as u32, HEIGHT as u32));

        let mut last_touch = None;
        loop {
            slint::platform::update_timers_and_animations();

            match touch.read() {
                Some(point) => {
                    let pos = point.to_logical(window.scale_factor());
                    let event = match last_touch.replace(pos) {
                        // A still finger is not an event, but the frame below
                        // must still be drawn while it is held down.
                        Some(previous) => {
                            (previous != pos).then_some(WindowEvent::PointerMoved { position: pos })
                        }
                        None => Some(WindowEvent::PointerPressed {
                            position: pos,
                            button: PointerEventButton::Left,
                        }),
                    };
                    if let Some(event) = event {
                        window.dispatch_event_with_result(event)?;
                    }
                }
                None => {
                    if let Some(pos) = last_touch.take() {
                        window.dispatch_event_with_result(WindowEvent::PointerReleased {
                            position: pos,
                            button: PointerEventButton::Left,
                        })?;
                        window.dispatch_event_with_result(WindowEvent::PointerExited)?;
                    }
                }
            }

            window.draw_if_needed(|renderer| {
                let region = renderer.render(&mut frame, WIDTH);
                for (origin, size) in region.iter() {
                    // Widened to even bounds; both panel dimensions are even,
                    // so this stays on screen.
                    let x = origin.x as usize & !1;
                    let y = origin.y as usize & !1;
                    let x_end = (origin.x as usize + size.width as usize).next_multiple_of(2);
                    let y_end = (origin.y as usize + size.height as usize).next_multiple_of(2);
                    panel.flush(&frame, x..x_end, y..y_end);
                }
            });
        }
    }
}

/// The RM690B0 behind quad SPI. Commands go out on one line as opcode 0x02
/// with the command in the middle byte of a 24-bit address; pixels go out on
/// four lines as opcode 0x32 with RAMWR (0x2C) in the same place.
struct Panel {
    spi: SpiDmaBus<'static, Blocking>,
    cs: Output<'static>,
    /// Pixels converted to the panel's big-endian RGB565, one transfer's worth.
    staging: Box<[u8; CHUNK]>,
}

impl Panel {
    /// Landscape, the long side across, which the panel's memory is not; the
    /// way up that LilyGO calls rotation 2.
    const MADCTL: u8 = 0x80 | 0x20; // MY | MV
    /// In landscape, the visible rows start 16 lines into the panel's memory.
    const ROW_OFFSET: usize = 16;
    /// LilyGO's default; the panel's maximum is 0xFF.
    const BRIGHTNESS: u8 = 175;

    fn init(&mut self, delay: &Delay) {
        // LilyGO's sequence, which they send twice to be sure it takes.
        const SEQUENCE: &[(u8, &[u8], u32)] = &[
            (0xFE, &[0x20], 0), // command page 0x20
            (0x26, &[0x0A], 0), // MIPI off
            (0x24, &[0x80], 0), // SPI writes RAM
            (0x5A, &[0x51], 0), // SWIRE for the BV6804 supply
            (0x5B, &[0x2E], 0),
            (0xFE, &[0x00], 0), // back to the user command page
            (0x3A, &[0x55], 0), // 16 bits per pixel
            (0xC2, &[0x00], 10),
            (0x35, &[0x00], 0), // tearing effect line on
            (0x51, &[0x00], 0), // brightness zero until the image is there
            (0x11, &[], 120),   // sleep out
            (0x29, &[], 10),    // display on
        ];
        for _ in 0..2 {
            for &(command, params, wait_ms) in SEQUENCE {
                self.command(command, params);
                if wait_ms > 0 {
                    delay.delay_millis(wait_ms);
                }
            }
        }
        self.command(0x36, &[Self::MADCTL]);
        self.command(0x51, &[Self::BRIGHTNESS]);
    }

    fn command(&mut self, command: u8, params: &[u8]) {
        self.cs.set_low();
        let result = self.spi.half_duplex_write(
            DataMode::Single,
            Command::_8Bit(0x02, DataMode::Single),
            Address::_24Bit(u32::from(command) << 8, DataMode::Single),
            0,
            params,
        );
        self.cs.set_high();
        if let Err(e) = result {
            warn!("Panel command {command:#04x} failed: {e:?}");
        }
    }

    /// Copies a rectangle of `frame` to the panel. The bounds must be even.
    fn flush(&mut self, frame: &[Rgb565Pixel], columns: Range<usize>, rows: Range<usize>) {
        let window = |range: &Range<usize>, offset: usize| {
            let [start_hi, start_lo] = ((range.start + offset) as u16).to_be_bytes();
            let [end_hi, end_lo] = ((range.end - 1 + offset) as u16).to_be_bytes();
            [start_hi, start_lo, end_hi, end_lo]
        };
        self.command(0x2A, &window(&columns, 0));
        self.command(0x2B, &window(&rows, Self::ROW_OFFSET));

        self.cs.set_low();
        let mut first = true;
        let mut filled = 0;
        for row in rows {
            for pixel in &frame[row * WIDTH..][columns.clone()] {
                self.staging[filled..filled + 2].copy_from_slice(&pixel.0.to_be_bytes());
                filled += 2;
                if filled == CHUNK {
                    self.write_pixels(first, filled);
                    first = false;
                    filled = 0;
                }
            }
        }
        if filled > 0 {
            self.write_pixels(first, filled);
        }
        self.cs.set_high();
    }

    /// Sends `len` staged bytes. Only the first transfer of a run carries the
    /// opcode; the rest continue it while chip select stays low.
    fn write_pixels(&mut self, first: bool, len: usize) {
        let (command, address) = if first {
            (Command::_8Bit(0x32, DataMode::Single), Address::_24Bit(0x2C << 8, DataMode::Single))
        } else {
            (Command::None, Address::None)
        };
        if let Err(e) =
            self.spi.half_duplex_write(DataMode::Quad, command, address, 0, &self.staging[..len])
        {
            warn!("Panel pixel write failed: {e:?}");
        }
    }
}

/// The CST226SE, polled. Only the first finger is reported.
struct Touch {
    i2c: I2c<'static, Blocking>,
}

impl Touch {
    const ADDRESS: u8 = 0x5A;

    /// The first finger's position in panel pixels, if one is down.
    fn read(&mut self) -> Option<PhysicalPosition> {
        let mut report = [0u8; 7];
        self.i2c.write_read(Self::ADDRESS, &[0x00], &mut report).ok()?;
        let fingers = report[5] & 0x7F;
        // Byte 6 is a fixed marker; the others rule out empty reports and the
        // controller's "home button" area below the panel.
        if report[6] != 0xAB || report[0] == 0xAB || report[0] == 0x00 || report[5] == 0x80 {
            return None;
        }
        if fingers == 0 || fingers > 5 {
            // Out of step; this resynchronizes the controller.
            let _ = self.i2c.write(Self::ADDRESS, &[0x00, 0xAB]);
            return None;
        }
        // 12-bit coordinates in the controller's portrait orientation.
        let raw_x = (u16::from(report[1]) << 4) | u16::from(report[3] >> 4);
        let raw_y = (u16::from(report[2]) << 4) | u16::from(report[3] & 0x0F);
        // Turned to match the panel's landscape MADCTL.
        let x = WIDTH as i32 - i32::from(raw_y);
        let y = i32::from(raw_x);
        Some(PhysicalPosition::new(x.clamp(0, WIDTH as i32 - 1), y.clamp(0, HEIGHT as i32 - 1)))
    }
}
