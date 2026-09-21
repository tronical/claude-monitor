//! The provisioned settings in flash.
//!
//! One record at the start of the `nvs` partition from espflash's default
//! partition table. Nothing else uses it: the WiFi driver runs with its own
//! NVS support turned off.

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use log::{info, warn};

use crate::config::{Credentials, MAX_RECORD_LEN};

/// `nvs` in espflash's default partition table: 0x9000, 24 KiB.
const PARTITION_OFFSET: u32 = 0x9000;
const SECTOR_SIZE: u32 = 4096;

/// Written in place of a record by "reconfigure": forces setup on the next
/// boot even when the firmware has build-time credentials to fall back to.
const SETUP_REQUESTED: &[u8; 4] = b"CSET";

pub enum Stored {
    Credentials(Credentials),
    SetupRequested,
    Nothing,
}

pub struct Store {
    flash: FlashStorage<'static>,
}

impl Store {
    pub fn new(flash: FLASH<'static>) -> Self {
        // Writing flash stalls every instruction fetch from it. The other core
        // runs from flash too, so it has to be parked for the duration. Parking
        // is only safe once that core has stopped itself: writers must go
        // through `state::halt_ui_core` first. Reads at boot happen before the
        // second core exists.
        Self { flash: FlashStorage::new(flash).multicore_auto_park() }
    }

    pub fn load(&mut self) -> Stored {
        let mut buffer = [0u8; MAX_RECORD_LEN.next_multiple_of(4)];
        if let Err(e) = self.flash.read(PARTITION_OFFSET, &mut buffer) {
            warn!("Reading stored settings failed: {e:?}");
            return Stored::Nothing;
        }
        if buffer.starts_with(SETUP_REQUESTED) {
            return Stored::SetupRequested;
        }
        match Credentials::from_record(&buffer) {
            Some(credentials) => Stored::Credentials(credentials),
            None => Stored::Nothing,
        }
    }

    pub fn save(&mut self, credentials: &Credentials) -> Result<(), ()> {
        self.replace(&credentials.to_record())?;
        info!("Settings saved for network '{}'", credentials.ssid);
        Ok(())
    }

    pub fn request_setup(&mut self) -> Result<(), ()> {
        self.replace(SETUP_REQUESTED)
    }

    fn replace(&mut self, contents: &[u8]) -> Result<(), ()> {
        // NOR flash writes are word aligned; pad with the erased value.
        let mut padded = [0xFFu8; MAX_RECORD_LEN.next_multiple_of(4)];
        padded[..contents.len()].copy_from_slice(contents);
        let len = contents.len().next_multiple_of(4);

        self.flash
            .erase(PARTITION_OFFSET, PARTITION_OFFSET + SECTOR_SIZE)
            .and_then(|()| self.flash.write(PARTITION_OFFSET, &padded[..len]))
            .map_err(|e| warn!("Writing settings failed: {e:?}"))
    }
}
