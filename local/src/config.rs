//! Launcher-to-native wallet configuration. Never print connection credentials.
use sats_arena_core::{Error, Result};
use serde::Deserialize;
use std::fmt;
#[cfg(not(test))]
use std::{
    io::{BufRead, Read},
    time::Duration,
};

const MAX_CONFIG_BYTES: u64 = 16 * 1024;

pub(crate) struct Secret(String);
impl Secret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted NWC connection]")
    }
}
#[derive(Debug)]
pub(crate) enum WalletConfig {
    Fake,
    Nwc(Secret),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherConfig {
    version: u32,
    backend: String,
    #[serde(default)]
    nwc_uri: Option<String>,
}
fn rejected(message: &str) -> Error {
    Error::Rejected(message.into())
}
fn backend(value: &str) -> Result<&str> {
    match value {
        "fake" | "nwc" => Ok(value),
        _ => Err(rejected("Wallet backend must be fake or nwc")),
    }
}
fn parse_launcher(bytes: &[u8]) -> Result<LauncherConfig> {
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(rejected("Launcher wallet configuration is too large"));
    }
    let config: LauncherConfig = serde_json::from_slice(bytes)
        .map_err(|_| rejected("Invalid launcher wallet configuration"))?;
    if config.version != 1 {
        return Err(rejected(
            "Unsupported launcher wallet configuration version",
        ));
    }
    backend(&config.backend)?;
    if config.backend == "fake" && config.nwc_uri.as_ref().is_some_and(|uri| !uri.is_empty()) {
        return Err(rejected(
            "Fake-wallet configuration cannot include an NWC connection",
        ));
    }
    Ok(config)
}
fn resolve(
    launcher: Option<LauncherConfig>,
    dev_backend: Option<&str>,
    dev_uri: Option<&str>,
) -> Result<WalletConfig> {
    let explicit = dev_backend.map(backend).transpose()?;
    let dev_uri = dev_uri.filter(|s| !s.trim().is_empty());
    if explicit == Some("fake") && dev_uri.is_some() {
        return Err(rejected(
            "Conflicting development wallet settings: fake backend and NWC connection",
        ));
    }
    let selected = explicit.unwrap_or_else(|| {
        if dev_uri.is_some() {
            "nwc"
        } else {
            launcher
                .as_ref()
                .map(|l| l.backend.as_str())
                .unwrap_or("fake")
        }
    });
    if selected == "fake" {
        return Ok(WalletConfig::Fake);
    }
    let uri = dev_uri
        .map(str::to_owned)
        .or_else(|| launcher.and_then(|l| l.nwc_uri))
        .ok_or_else(|| {
            rejected(
                "NWC selected but no connection was supplied by the launcher or SATS_ARENA_NWC_URI",
            )
        })?;
    let uri = uri.trim();
    if !uri.starts_with("nostr+walletconnect://")
        || uri.len() > 8192
        || uri.chars().any(char::is_whitespace)
    {
        return Err(rejected(
            "Invalid NWC connection; update the saved launcher connection or SATS_ARENA_NWC_URI",
        ));
    }
    Ok(WalletConfig::Nwc(Secret(uri.to_owned())))
}
#[cfg(not(test))]
fn read_launcher() -> Result<LauncherConfig> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("arena-wallet-config".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut input = stdin.lock().take(MAX_CONFIG_BYTES + 1);
            let mut bytes = Vec::new();
            let result = input
                .read_until(b'\n', &mut bytes)
                .map_err(|_| rejected("Could not read launcher wallet configuration"))
                .and_then(|_| parse_launcher(&bytes));
            let _ = sender.send(result);
        })
        .map_err(|_| rejected("Could not start launcher wallet configuration reader"))?;
    receiver
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| rejected("Launcher did not supply wallet configuration in time"))?
}
#[cfg(not(test))]
pub(crate) fn load() -> Result<WalletConfig> {
    let launcher = match std::env::var("SATS_ARENA_WALLET_CONFIG") {
        Ok(value) if value == "stdin" => Some(read_launcher()?),
        Ok(_) => return Err(rejected("SATS_ARENA_WALLET_CONFIG must be stdin when set")),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err(rejected("Invalid launcher wallet configuration selector")),
    };
    let dev_backend = std::env::var("SATS_ARENA_WALLET_BACKEND").ok();
    let dev_uri = std::env::var("SATS_ARENA_NWC_URI").ok();
    resolve(launcher, dev_backend.as_deref(), dev_uri.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn handoff(backend: &str, uri: Option<&str>) -> LauncherConfig {
        LauncherConfig {
            version: 1,
            backend: backend.into(),
            nwc_uri: uri.map(str::to_owned),
        }
    }
    #[test]
    fn saved_connection_is_default_and_explicit_development_uri_wins() {
        let saved = "nostr+walletconnect://saved?secret=do-not-print";
        let dev = "nostr+walletconnect://development?secret=also-private";
        let WalletConfig::Nwc(uri) =
            resolve(Some(handoff("nwc", Some(saved))), None, None).unwrap()
        else {
            panic!()
        };
        assert_eq!(uri.expose(), saved);
        let WalletConfig::Nwc(uri) =
            resolve(Some(handoff("nwc", Some(saved))), None, Some(dev)).unwrap()
        else {
            panic!()
        };
        assert_eq!(uri.expose(), dev);
        assert!(!format!("{uri:?}").contains("also-private"));
        assert!(matches!(
            resolve(None, None, None).unwrap(),
            WalletConfig::Fake
        ));
    }
    #[test]
    fn unavailable_or_malformed_real_configuration_never_falls_back_to_fake() {
        assert!(resolve(Some(handoff("nwc", None)), None, None).is_err());
        let err = resolve(None, Some("nwc"), Some("secret-that-is-not-a-uri")).unwrap_err();
        assert!(!err.to_string().contains("secret-that"));
        assert!(resolve(None, Some("fake"), Some("nostr+walletconnect://secret")).is_err());
        assert!(parse_launcher(br#"{"version":2,"backend":"nwc","nwc_uri":"secret"}"#).is_err());
        assert!(parse_launcher(br#"{"version":1,"backend":"fake","nwc_uri":"secret"}"#).is_err());
        assert!(parse_launcher(&vec![b'x'; MAX_CONFIG_BYTES as usize + 1]).is_err());
        assert!(parse_launcher(b"private malformed data").is_err());
    }
}
