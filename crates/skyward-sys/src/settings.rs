//! The phone's own settings, as the recovery reads them back from the
//! installed system: which form the clock takes, and how fast the panel
//! runs. They live in Android's settings files in the data partition, which
//! reads only once it is decrypted; before that, or where the phone keeps
//! no setting, the recovery falls back to what it can tell.

use std::fs;
use std::path::Path;

/// Where Android keeps the settings its user can change, for the phone's
/// first user.
const SYSTEM: &str = "/data/system/users/0/settings_system.xml";
const SECURE: &str = "/data/system/users/0/settings_secure.xml";

/// The setting that says which form the clock takes: `12` or `24`.
const HOUR_FORMAT: &str = "time_12_24";

/// The settings that say how fast the panel runs, widest first: Xiaomi's
/// own, then the two Android keeps.
const REFRESH: [&str; 3] = ["miui_refresh_rate", "peak_refresh_rate", "user_refresh_rate"];

/// Whether the clock runs on a 24-hour clock, as the installed system says;
/// `None` where the settings cannot be read or say nothing, which a caller
/// then decides by the language the pages speak.
pub fn hour24() -> Option<bool> {
    match value(&fs::read(SYSTEM).ok()?, HOUR_FORMAT)?.as_str() {
        "24" => Some(true),
        "12" => Some(false),
        _ => None,
    }
}

/// How fast the panel runs, in hertz, as the installed system says; `None`
/// where the settings cannot be read or say nothing.
pub fn refresh_rate() -> Option<u32> {
    let system = fs::read(SYSTEM).ok();
    let secure = fs::read(SECURE).ok();
    REFRESH.iter().find_map(|name| {
        let said = system.as_deref().and_then(|raw| value(raw, name)).or_else(|| secure.as_deref().and_then(|raw| value(raw, name)))?;
        // Android keeps these as floats on some builds, `120.0`.
        said.trim_end_matches('f').parse::<f64>().ok().map(|hz| hz.round() as u32).filter(|&hz| hz > 0)
    })
}

/// What the settings file `raw` holds under `name`, as text.
///
/// Android writes its settings as a compiled XML document, whose strings
/// sit in one pool. An entry's name and its value are the first two strings
/// after the entry, so the value is the run of text that follows the name
/// in that pool: the name, its zero byte, then the value.
fn value(raw: &[u8], name: &str) -> Option<String> {
    let needle = name.as_bytes();
    // The name must sit on a string's own start and end, so a longer name
    // that begins or ends in this one is not taken for it.
    let at = raw
        .windows(needle.len())
        .enumerate()
        .find(|(i, w)| w == &needle && (*i == 0 || raw[*i - 1] == 0) && raw.get(i + needle.len()) == Some(&0))
        .map(|(i, _)| i)?;
    let start = at + name.len() + 1;
    let end = start + raw.get(start..)?.iter().position(|&b| b == 0)?;
    let said = String::from_utf8(raw.get(start..end)?.to_vec()).ok()?;
    // A value is a run of text a setting holds, not another name: one that
    // is empty says nothing.
    (!said.is_empty()).then_some(said)
}

/// Whether the settings files read at all, which they do once the data
/// partition is decrypted.
pub fn readable() -> bool {
    Path::new(SYSTEM).try_exists().unwrap_or(false) || Path::new(SECURE).try_exists().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings file holding `entries` as Android's compiled XML lays
    /// strings out: each run of text ending in a zero byte, an entry's name
    /// followed by its value.
    fn settings(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut raw = b"\x03\x00\x08\x00".to_vec();
        for (name, value) in entries {
            raw.extend_from_slice(name.as_bytes());
            raw.push(0);
            raw.extend_from_slice(value.as_bytes());
            raw.push(0);
            raw.extend_from_slice(b"android");
            raw.push(0);
            raw.extend_from_slice(value.as_bytes());
            raw.push(0);
        }
        raw
    }

    #[test]
    fn a_setting_is_the_text_after_its_name() {
        let raw = settings(&[("time_12_24", "12"), ("peak_refresh_rate", "120")]);
        assert_eq!(value(&raw, "time_12_24").as_deref(), Some("12"));
        assert_eq!(value(&raw, "peak_refresh_rate").as_deref(), Some("120"));
        assert_eq!(value(&raw, "miui_refresh_rate"), None, "a setting the file does not hold");
    }

    #[test]
    fn a_longer_name_that_ends_in_this_one_is_not_taken_for_it() {
        let raw = settings(&[("peak_refresh_rate_backup", "60")]);
        assert_eq!(value(&raw, "peak_refresh_rate"), None, "the name is taken as the string it is");
    }

    #[test]
    fn twelve_and_twenty_four_are_the_two_forms_a_clock_takes() {
        assert_eq!(form("24"), Some(true));
        assert_eq!(form("12"), Some(false));
        assert_eq!(form(""), None);
    }

    /// What a `time_12_24` value comes to, as the reading decides it.
    fn form(said: &str) -> Option<bool> {
        match said {
            "24" => Some(true),
            "12" => Some(false),
            _ => None,
        }
    }
}
