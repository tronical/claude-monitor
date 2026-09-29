//! The provisioned settings: how they are stored, and how the setup form's
//! submission is decoded into them.
//!
//! Nothing here touches hardware, so it can be tested on the host:
//!
//! ```sh
//! rustc +stable --edition 2024 --test src/config.rs -o target/config-tests && target/config-tests
//! ```

#![cfg_attr(test, allow(dead_code))]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
#[cfg(test)]
use alloc::{format, vec};

/// WiFi SSIDs are at most 32 bytes, WPA2 passphrases at most 63.
const MAX_SSID: usize = 32;
const MAX_PASSWORD: usize = 63;
/// Generous: today's tokens are a little over a hundred characters.
const MAX_TOKEN: usize = 512;
/// Home, office, and a few more; the form refuses beyond this.
pub const MAX_NETWORKS: usize = 8;

const MAGIC: &[u8; 4] = b"CMON";
/// Version 1 had no flags byte and version 2 a single network; both are
/// still read.
const FORMAT_VERSION: u8 = 3;
/// Set on a record left behind by "add a network": the next boot goes to
/// setup with these settings on offer, the token kept unless replaced.
const FLAG_SETUP_REQUESTED: u8 = 1;
/// Magic, version, flags, count, the networks, the token, CRC.
pub const MAX_RECORD_LEN: usize =
    4 + 1 + 1 + 1 + MAX_NETWORKS * (2 + MAX_SSID + 2 + MAX_PASSWORD) + 2 + MAX_TOKEN + 4;

#[derive(Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    pub password: String,
}

// Deliberately not derived: a `{:?}` in a log line must not leak secrets.
impl core::fmt::Debug for Network {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Network").field("ssid", &self.ssid).finish_non_exhaustive()
    }
}

/// Everything the box needs to run: the networks it may join, and the token.
#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    /// In the order they were added; never empty once validated.
    pub networks: Vec<Network>,
    pub token: String,
}

impl core::fmt::Debug for Settings {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Settings").field("networks", &self.networks).finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    MissingSsid,
    SsidTooLong,
    /// WPA2 passphrases are 8 to 63 characters; empty means an open network.
    PasswordLength,
    TooManyNetworks,
    MissingToken,
    /// Not shaped like a token at all, most likely a paste accident.
    TokenShape,
}

impl Invalid {
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingSsid => "Enter the name of a WiFi network, or keep at least one.",
            Self::SsidTooLong => "WiFi network names are at most 32 characters.",
            Self::PasswordLength => {
                "WiFi passwords are 8 to 63 characters. Leave it empty for an open network."
            }
            Self::TooManyNetworks => "Eight networks are stored already; forget one first.",
            Self::MissingToken => "Paste the token printed by `claude setup-token`.",
            Self::TokenShape => {
                "That does not look like a token: it should be a single line without spaces, \
                 starting with sk-ant-."
            }
        }
    }
}

impl Network {
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.ssid.is_empty() {
            return Err(Invalid::MissingSsid);
        }
        if self.ssid.len() > MAX_SSID {
            return Err(Invalid::SsidTooLong);
        }
        if !(self.password.is_empty() || (8..=MAX_PASSWORD).contains(&self.password.len())) {
            return Err(Invalid::PasswordLength);
        }
        Ok(())
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.networks.is_empty() {
            return Err(Invalid::MissingSsid);
        }
        if self.networks.len() > MAX_NETWORKS {
            return Err(Invalid::TooManyNetworks);
        }
        for network in &self.networks {
            network.validate()?;
        }
        if self.token.is_empty() {
            return Err(Invalid::MissingToken);
        }
        // The token goes into an HTTP header verbatim, so control characters
        // and whitespace are not just wrong but a header injection.
        if self.token.len() > MAX_TOKEN
            || !self.token.starts_with("sk-ant-")
            || !self.token.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(Invalid::TokenShape);
        }
        Ok(())
    }

    /// Decode an `application/x-www-form-urlencoded` submission on top of
    /// what is stored: `forget` (repeatable) drops networks by name, a
    /// non-empty `ssid` with `password` adds one or replaces the one of that
    /// name, and an empty `token` keeps the stored one when there is one.
    pub fn from_form(body: &str, previous: Option<&Self>) -> Result<Self, Invalid> {
        let mut ssid = String::new();
        let mut password = String::new();
        let mut token = String::new();
        let mut forget: Vec<String> = Vec::new();
        for pair in body.split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = url_decode(value);
            match name {
                // Passwords may legitimately start or end with a space; SSIDs
                // can too, but a stray one from autocomplete is far likelier.
                "ssid" => ssid = String::from(value.trim()),
                "password" => password = value,
                "token" => token = String::from(value.trim()),
                "forget" => forget.push(value),
                _ => {}
            }
        }

        let mut networks: Vec<Network> = previous
            .map(|p| p.networks.iter().filter(|n| !forget.contains(&n.ssid)).cloned().collect())
            .unwrap_or_default();
        if !ssid.is_empty() {
            let network = Network { ssid, password };
            network.validate()?;
            match networks.iter_mut().find(|n| n.ssid == network.ssid) {
                Some(existing) => *existing = network,
                None => networks.push(network),
            }
        }
        if token.is_empty()
            && let Some(previous) = previous
        {
            token = previous.token.clone();
        }

        let settings = Self { networks, token };
        settings.validate()?;
        Ok(settings)
    }

    pub fn to_record(&self, setup_requested: bool) -> Vec<u8> {
        let mut record = Vec::with_capacity(MAX_RECORD_LEN);
        record.extend_from_slice(MAGIC);
        record.push(FORMAT_VERSION);
        record.push(if setup_requested { FLAG_SETUP_REQUESTED } else { 0 });
        record.push(self.networks.len() as u8);
        let mut field = |text: &str| {
            record.extend_from_slice(&(text.len() as u16).to_le_bytes());
            record.extend_from_slice(text.as_bytes());
        };
        for network in &self.networks {
            field(&network.ssid);
            field(&network.password);
        }
        field(&self.token);
        let crc = crc32(&record);
        record.extend_from_slice(&crc.to_le_bytes());
        record
    }

    /// Decode a record from the start of `flash`, which may be followed by
    /// anything, along with whether it asks for setup. Erased flash, a torn
    /// write and a future format all read as `None`, which sends the device
    /// back to setup.
    pub fn from_record(flash: &[u8]) -> Option<(Self, bool)> {
        let mut cursor = flash;
        if take(&mut cursor, MAGIC.len())? != MAGIC {
            return None;
        }
        let version = take(&mut cursor, 1)?[0];
        let flags = match version {
            1 => 0,
            2 | FORMAT_VERSION => take(&mut cursor, 1)?[0],
            _ => return None,
        };
        let count = match version {
            FORMAT_VERSION => usize::from(take(&mut cursor, 1)?[0]),
            _ => 1,
        };
        let mut field = || {
            let len = u16::from_le_bytes(take(&mut cursor, 2)?.try_into().ok()?);
            String::from_utf8(take(&mut cursor, len.into())?.to_vec()).ok()
        };
        let mut networks = Vec::with_capacity(count.min(MAX_NETWORKS));
        for _ in 0..count {
            networks.push(Network { ssid: field()?, password: field()? });
        }
        let settings = Self { networks, token: field()? };

        let covered = flash.len() - cursor.len();
        let stored_crc = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().ok()?);
        (crc32(&flash[..covered]) == stored_crc && settings.validate().is_ok())
            .then_some((settings, flags & FLAG_SETUP_REQUESTED != 0))
    }
}

fn take<'a>(cursor: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let (head, tail) = cursor.split_at_checked(len)?;
    *cursor = tail;
    Some(head)
}

/// Percent-decoding as browsers encode forms: `+` is a space. Malformed
/// escapes are kept literally rather than rejected.
pub fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        match bytes[i] {
            b'+' => decoded.push(b' '),
            b'%' if i + 2 < bytes.len()
                && hex(bytes[i + 1]).is_some()
                && hex(bytes[i + 2]).is_some() =>
            {
                decoded.push(hex(bytes[i + 1]).unwrap() << 4 | hex(bytes[i + 2]).unwrap());
                i += 2;
            }
            other => decoded.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// CRC-32 (IEEE), bitwise. The record is a few hundred bytes, written once.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

/// Escape text for HTML element content and double-quoted attributes.
pub fn html_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Escape the characters that are special in a `WIFI:` QR code payload.
pub fn wifi_qr_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | ';' | ',' | ':' | '"') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "sk-ant-oat01-abcDEF_123-xyz";

    fn net(ssid: &str, password: &str) -> Network {
        Network { ssid: ssid.into(), password: password.into() }
    }

    fn sample() -> Settings {
        Settings { networks: vec![net("Café Net", "p&ss word+1")], token: TOKEN.into() }
    }

    fn two() -> Settings {
        Settings { networks: vec![net("home", "homepass1"), net("office", "")], token: TOKEN.into() }
    }

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn record_round_trips_and_ignores_trailing_flash() {
        let mut flash = two().to_record(false);
        assert!(flash.len() <= MAX_RECORD_LEN);
        flash.extend_from_slice(&[0xFF; 64]);
        assert_eq!(Settings::from_record(&flash), Some((two(), false)));
        assert_eq!(Settings::from_record(&two().to_record(true)), Some((two(), true)));
    }

    #[test]
    fn a_full_record_fits() {
        let full = Settings {
            networks: (0..MAX_NETWORKS)
                .map(|i| net(&format!("{i}{}", "s".repeat(MAX_SSID - 1)), &"p".repeat(MAX_PASSWORD)))
                .collect(),
            token: format!("sk-ant-{}", "x".repeat(MAX_TOKEN - 7)),
        };
        let record = full.to_record(false);
        assert!(record.len() <= MAX_RECORD_LEN);
        assert_eq!(Settings::from_record(&record), Some((full, false)));
    }

    /// What firmware before the flags byte (v1) and before several networks
    /// (v2) wrote.
    fn old_record(version: u8) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(MAGIC);
        record.push(version);
        if version == 2 {
            record.push(0);
        }
        let s = sample();
        for field in [&s.networks[0].ssid, &s.networks[0].password, &s.token] {
            record.extend_from_slice(&(field.len() as u16).to_le_bytes());
            record.extend_from_slice(field.as_bytes());
        }
        let crc = crc32(&record);
        record.extend_from_slice(&crc.to_le_bytes());
        record
    }

    #[test]
    fn older_records_still_read() {
        assert_eq!(Settings::from_record(&old_record(1)), Some((sample(), false)));
        assert_eq!(Settings::from_record(&old_record(2)), Some((sample(), false)));
    }

    #[test]
    fn erased_torn_and_corrupt_flash_read_as_unprovisioned() {
        assert_eq!(Settings::from_record(&[0xFF; 256]), None);
        assert_eq!(Settings::from_record(&[]), None);

        let record = sample().to_record(false);
        assert_eq!(Settings::from_record(&record[..record.len() - 1]), None);

        let mut flipped = record.clone();
        flipped[12] ^= 1;
        assert_eq!(Settings::from_record(&flipped), None);

        let mut future = record;
        future[4] = FORMAT_VERSION + 1;
        assert_eq!(Settings::from_record(&future), None);
    }

    #[test]
    fn decodes_a_browser_form_submission() {
        let body = "ssid=Caf%C3%A9+Net&password=p%26ss+word%2B1&token=++sk-ant-oat01-abcDEF_123-xyz%0D%0A";
        assert_eq!(Settings::from_form(body, None), Ok(sample()));
    }

    #[test]
    fn a_form_edits_the_stored_networks() {
        let prev = two();
        // Add one, keep the token.
        let added = Settings::from_form("ssid=lab&password=labpass99&token=", Some(&prev)).unwrap();
        assert_eq!(added.networks.len(), 3);
        assert_eq!(added.networks[2], net("lab", "labpass99"));
        assert_eq!(added.token, TOKEN);
        // Replace a password by re-adding the same name.
        let replaced = Settings::from_form("ssid=home&password=newpass99", Some(&prev)).unwrap();
        assert_eq!(replaced.networks, vec![net("home", "newpass99"), net("office", "")]);
        // Forget one, add none.
        let fewer = Settings::from_form("forget=office&ssid=&password=", Some(&prev)).unwrap();
        assert_eq!(fewer.networks, vec![net("home", "homepass1")]);
        // Forget all, add none: nothing left to join.
        assert_eq!(
            Settings::from_form("forget=home&forget=office", Some(&prev)),
            Err(Invalid::MissingSsid)
        );
    }

    #[test]
    fn an_empty_token_keeps_the_old_one_only_when_there_is_one() {
        assert_eq!(Settings::from_form("ssid=x&password=&token=", Some(&sample())).map(|s| s.token), Ok(TOKEN.into()));
        assert_eq!(Settings::from_form("ssid=x&password=&token=", None), Err(Invalid::MissingToken));
        // A new token still wins over a kept one.
        assert_eq!(
            Settings::from_form("ssid=x&password=&token=sk-ant-new", Some(&sample())).map(|s| s.token),
            Ok("sk-ant-new".into())
        );
    }

    #[test]
    fn refuses_a_ninth_network() {
        let full = Settings {
            networks: (0..MAX_NETWORKS).map(|i| net(&format!("n{i}"), "")).collect(),
            token: TOKEN.into(),
        };
        assert_eq!(Settings::from_form("ssid=one-more&password=", Some(&full)), Err(Invalid::TooManyNetworks));
        // Unless one goes at the same time.
        assert!(Settings::from_form("forget=n0&ssid=one-more&password=", Some(&full)).is_ok());
    }

    #[test]
    fn malformed_percent_escapes_are_kept_literally() {
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz%4"), "%zz%4");
        assert_eq!(url_decode("%41%"), "A%");
    }

    #[test]
    fn rejects_what_would_not_work() {
        let form = |ssid: &str, password: &str, token: &str| {
            Settings { networks: vec![net(ssid, password)], token: token.into() }.validate()
        };
        assert_eq!(form("", "password", TOKEN), Err(Invalid::MissingSsid));
        assert_eq!(form(&"x".repeat(33), "password", TOKEN), Err(Invalid::SsidTooLong));
        assert_eq!(form("net", "short", TOKEN), Err(Invalid::PasswordLength));
        assert_eq!(form("net", "", TOKEN), Ok(()));
        assert_eq!(form("net", "password", ""), Err(Invalid::MissingToken));
        assert_eq!(form("net", "password", "hunter2"), Err(Invalid::TokenShape));
    }

    #[test]
    fn a_token_cannot_smuggle_a_header() {
        let injected = Settings { networks: vec![net("net", "")], token: "sk-ant-x\r\nX-Evil: 1".into() };
        assert_eq!(injected.validate(), Err(Invalid::TokenShape));
    }

    #[test]
    fn debug_output_has_no_secrets() {
        let printed = alloc::format!("{:?}", sample());
        assert!(!printed.contains(TOKEN) && !printed.contains("p&ss"));
    }

    #[test]
    fn escapes_for_html_and_wifi_qr() {
        assert_eq!(html_escape(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(wifi_qr_escape(r#"my;net:"1",\"#), r#"my\;net\:\"1\"\,\\"#);
    }
}
