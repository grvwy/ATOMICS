//! This browser's identity toward a host it paired with: the device's Noise static key, kept per host
//! (the browser glue stores it; see `web/device-store.js`), and the name the host's pairing prompt
//! shows for it.

use std::fmt;

use atomics_core::noise::{PublicKey, Secret};

/// A host this browser paired with, and the device key it paired with.
#[derive(Clone)]
pub struct Device {
    pub host_id: String,
    /// The host's Noise static key, as the pairing link gave it.
    pub host_key: PublicKey,
    pub secret: Secret,
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device").field("host_id", &self.host_id).finish_non_exhaustive()
    }
}

/// How the host's pairing prompt names this browser: "Safari on iPhone", read from its user agent.
pub fn name(user_agent: &str) -> String {
    let has = |needle: &str| user_agent.contains(needle);
    let browser = if has("Edg/") || has("EdgiOS/") || has("EdgA/") {
        "Edge"
    } else if has("Firefox/") || has("FxiOS/") {
        "Firefox"
    } else if has("OPR/") {
        "Opera"
    } else if has("CriOS/") || has("Chrome/") {
        "Chrome"
    } else if has("Safari/") {
        "Safari"
    } else {
        "A browser"
    };
    let platform = if has("iPhone") {
        Some("iPhone")
    } else if has("iPad") {
        Some("iPad")
    } else if has("Android") {
        Some("Android")
    } else if has("CrOS") {
        Some("ChromeOS")
    } else if has("Macintosh") {
        Some("Mac")
    } else if has("Windows") {
        Some("Windows")
    } else if has("Linux") {
        Some("Linux")
    } else {
        None
    };
    match platform {
        Some(platform) => format!("{browser} on {platform}"),
        None => browser.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_is_named_by_its_browser_and_platform() {
        let cases = [
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 26_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1",
                "Safari on iPhone",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 26_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/140.0 Mobile/15E148 Safari/604.1",
                "Chrome on iPhone",
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36",
                "Chrome on Linux",
            ),
            (
                "Mozilla/5.0 (Linux; Android 15) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Mobile Safari/537.36",
                "Chrome on Android",
            ),
            ("Mozilla/5.0 (Macintosh; Intel Mac OS X 15.6; rv:143.0) Gecko/20100101 Firefox/143.0", "Firefox on Mac"),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36 Edg/141.0",
                "Edge on Windows",
            ),
            ("", "A browser"),
        ];
        for (agent, expected) in cases {
            assert_eq!(name(agent), expected, "{agent}");
        }
    }

    #[test]
    fn debug_never_shows_the_key() {
        let secret = Secret::generate().unwrap();
        let device = Device { host_id: "h".into(), host_key: secret.public(), secret };
        assert_eq!(format!("{device:?}"), "Device { host_id: \"h\", .. }");
    }
}
