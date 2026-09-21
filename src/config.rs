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

/// WiFi SSIDs are at most 32 bytes, WPA2 passphrases at most 63.
const MAX_SSID: usize = 32;
const MAX_PASSWORD: usize = 63;
/// Generous: today's tokens are a little over a hundred characters.
const MAX_TOKEN: usize = 512;

const MAGIC: &[u8; 4] = b"CMON";
const FORMAT_VERSION: u8 = 1;
/// Magic, version, three length-prefixed fields, CRC.
pub const MAX_RECORD_LEN: usize = 4 + 1 + 3 * 2 + MAX_SSID + MAX_PASSWORD + MAX_TOKEN + 4;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub ssid: String,
    pub password: String,
    pub token: String,
}

// Deliberately not derived: a `{:?}` in a log line must not leak secrets.
impl core::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Credentials").field("ssid", &self.ssid).finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    MissingSsid,
    SsidTooLong,
    /// WPA2 passphrases are 8 to 63 characters; empty means an open network.
    PasswordLength,
    MissingToken,
    /// Not shaped like a token at all, most likely a paste accident.
    TokenShape,
}

impl Invalid {
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingSsid => "Enter the name of your WiFi network.",
            Self::SsidTooLong => "WiFi network names are at most 32 characters.",
            Self::PasswordLength => {
                "WiFi passwords are 8 to 63 characters. Leave it empty for an open network."
            }
            Self::MissingToken => "Paste the token printed by `claude setup-token`.",
            Self::TokenShape => {
                "That does not look like a token: it should be a single line without spaces, \
                 starting with sk-ant-."
            }
        }
    }
}

impl Credentials {
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

    /// Decode an `application/x-www-form-urlencoded` body with the fields
    /// `ssid`, `password` and `token`.
    pub fn from_form(body: &str) -> Result<Self, Invalid> {
        let mut credentials =
            Self { ssid: String::new(), password: String::new(), token: String::new() };
        for pair in body.split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = url_decode(value);
            match name {
                // Passwords may legitimately start or end with a space; SSIDs
                // can too, but a stray one from autocomplete is far likelier.
                "ssid" => credentials.ssid = String::from(value.trim()),
                "password" => credentials.password = value,
                "token" => credentials.token = String::from(value.trim()),
                _ => {}
            }
        }
        credentials.validate()?;
        Ok(credentials)
    }

    pub fn to_record(&self) -> Vec<u8> {
        let mut record = Vec::with_capacity(MAX_RECORD_LEN);
        record.extend_from_slice(MAGIC);
        record.push(FORMAT_VERSION);
        for field in [&self.ssid, &self.password, &self.token] {
            record.extend_from_slice(&(field.len() as u16).to_le_bytes());
            record.extend_from_slice(field.as_bytes());
        }
        let crc = crc32(&record);
        record.extend_from_slice(&crc.to_le_bytes());
        record
    }

    /// Decode a record from the start of `flash`, which may be followed by
    /// anything. Erased flash, a torn write and a future format all read as
    /// `None`, which sends the device back to setup.
    pub fn from_record(flash: &[u8]) -> Option<Self> {
        let mut cursor = flash;
        if take(&mut cursor, MAGIC.len())? != MAGIC || take(&mut cursor, 1)? != [FORMAT_VERSION] {
            return None;
        }
        let mut field = || {
            let len = u16::from_le_bytes(take(&mut cursor, 2)?.try_into().ok()?);
            String::from_utf8(take(&mut cursor, len.into())?.to_vec()).ok()
        };
        let credentials = Self { ssid: field()?, password: field()?, token: field()? };

        let covered = flash.len() - cursor.len();
        let stored_crc = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().ok()?);
        (crc32(&flash[..covered]) == stored_crc && credentials.validate().is_ok())
            .then_some(credentials)
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

    fn sample() -> Credentials {
        Credentials { ssid: "Café Net".into(), password: "p&ss word+1".into(), token: TOKEN.into() }
    }

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn record_round_trips_and_ignores_trailing_flash() {
        let mut flash = sample().to_record();
        assert!(flash.len() <= MAX_RECORD_LEN);
        flash.extend_from_slice(&[0xFF; 64]);
        assert_eq!(Credentials::from_record(&flash), Some(sample()));
    }

    #[test]
    fn erased_torn_and_corrupt_flash_read_as_unprovisioned() {
        assert_eq!(Credentials::from_record(&[0xFF; 256]), None);
        assert_eq!(Credentials::from_record(&[]), None);

        let record = sample().to_record();
        assert_eq!(Credentials::from_record(&record[..record.len() - 1]), None);

        let mut flipped = record.clone();
        flipped[12] ^= 1;
        assert_eq!(Credentials::from_record(&flipped), None);

        let mut future = record;
        future[4] = FORMAT_VERSION + 1;
        assert_eq!(Credentials::from_record(&future), None);
    }

    #[test]
    fn decodes_a_browser_form_submission() {
        let body = "ssid=Caf%C3%A9+Net&password=p%26ss+word%2B1&token=++sk-ant-oat01-abcDEF_123-xyz%0D%0A";
        assert_eq!(Credentials::from_form(body), Ok(sample()));
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
            Credentials { ssid: ssid.into(), password: password.into(), token: token.into() }
                .validate()
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
        let injected = Credentials {
            ssid: "net".into(),
            password: String::new(),
            token: "sk-ant-x\r\nX-Evil: 1".into(),
        };
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
