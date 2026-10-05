//! The board, picked with a cargo feature (`build.rs` insists on exactly one).
//! Each provides `entry`, the attribute for `main`, and `init`, which sets up
//! the display, touch, the Slint platform and the PSRAM heap.

#[cfg(feature = "esp32-s3-box-3")]
pub use mcu_board_support::{entry, init};

#[cfg(feature = "lilygo-t4-s3")]
mod lilygo_t4_s3;
#[cfg(feature = "lilygo-t4-s3")]
pub use lilygo_t4_s3::{entry, init};
