//! Runtime configuration: NVS-backed config written by BLE provisioning, with
//! the build-time `cfg.toml` (toml_cfg) as a factory-default fallback.
//!
//! Precedence is decided in `main`: an NVS provisioning record wins; otherwise a
//! complete baked `cfg.toml` is used (so boards flashed the old way keep working);
//! otherwise the leaf enters BLE provisioning mode and an app writes a payload
//! here via [`store_payload`]. The payload schema matches `@listam/provisioning`
//! (see listam-packages/packages/provisioning/index.mjs) and the firmware Config.

use anyhow::anyhow;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use serde::Deserialize;

use crate::Config;

/// NVS namespace + keys. Reuses the EspDefaultNvsPartition taken in `main`.
const NS: &str = "listam";
const KEY_PROVISIONED: &str = "provisioned";
const KEY_PAYLOAD: &str = "payload";
/// Matches PROVISIONING_PAYLOAD_VERSION in @listam/provisioning.
const PAYLOAD_VERSION: u32 = 1;
/// Generous upper bound for the stored JSON blob (~300B in practice).
const MAX_BLOB: usize = 1024;

/// Fully resolved configuration the normal boot path runs on (owned strings).
pub struct RuntimeConfig {
    pub networks: Vec<(String, String)>,
    pub hub_addr: String,
    pub control_key: [u8; 32],
    pub audio_addr: String,
    pub wake_db_threshold: i32,
    pub silence_timeout_ms: i32,
    pub led_gpio: i32,
}

#[derive(Deserialize)]
struct WifiNet {
    ssid: String,
    #[serde(default)]
    psk: String,
}

#[derive(Deserialize)]
struct Payload {
    v: u32,
    control_key: String,
    hub_addr: String,
    wifi: Vec<WifiNet>,
    #[serde(default)]
    audio_addr: Option<String>,
    #[serde(default)]
    wake_db_threshold: Option<i32>,
    #[serde(default)]
    silence_timeout_ms: Option<i32>,
    #[serde(default)]
    led_gpio: Option<i32>,
}

/// Why a provisioning payload was rejected — maps to the BLE status code the
/// leaf notifies back to the central.
pub enum ProvError {
    Decode(String),
    Validate(String),
    Nvs(String),
}

fn parse_key(hex_str: &str) -> Option<[u8; 32]> {
    hex::decode(hex_str.trim()).ok().and_then(|b| b.try_into().ok())
}

fn payload_to_runtime(p: &Payload) -> Result<RuntimeConfig, String> {
    if p.v != PAYLOAD_VERSION {
        return Err(format!("unsupported payload version {}", p.v));
    }
    let control_key = parse_key(&p.control_key).ok_or("control_key must be 64 hex chars")?;
    if p.hub_addr.trim().is_empty() {
        return Err("hub_addr is required".into());
    }
    let networks: Vec<(String, String)> = p
        .wifi
        .iter()
        .filter(|n| !n.ssid.is_empty())
        .map(|n| (n.ssid.clone(), n.psk.clone()))
        .collect();
    if networks.is_empty() {
        return Err("at least one wifi network is required".into());
    }
    Ok(RuntimeConfig {
        networks,
        hub_addr: p.hub_addr.trim().to_string(),
        control_key,
        audio_addr: p.audio_addr.clone().unwrap_or_default(),
        wake_db_threshold: p.wake_db_threshold.unwrap_or(-25),
        silence_timeout_ms: p.silence_timeout_ms.unwrap_or(800),
        led_gpio: p.led_gpio.unwrap_or(48),
    })
}

/// Decode + validate a received payload and persist it to NVS, setting the
/// `provisioned` flag. Returns a short control-key fingerprint for logging.
pub fn store_payload(nvs: &EspDefaultNvsPartition, raw: &[u8]) -> Result<String, ProvError> {
    let payload: Payload = serde_json::from_slice(raw).map_err(|e| ProvError::Decode(e.to_string()))?;
    // Validate semantics before writing anything.
    payload_to_runtime(&payload).map_err(ProvError::Validate)?;

    let mut store = EspNvs::new(nvs.clone(), NS, true).map_err(|e| ProvError::Nvs(e.to_string()))?;
    store.set_blob(KEY_PAYLOAD, raw).map_err(|e| ProvError::Nvs(e.to_string()))?;
    store.set_u8(KEY_PROVISIONED, 1).map_err(|e| ProvError::Nvs(e.to_string()))?;

    Ok(payload.control_key.chars().take(8).collect())
}

/// Load the persisted runtime config, or `None` if the leaf has never been
/// provisioned over BLE.
pub fn load(nvs: &EspDefaultNvsPartition) -> anyhow::Result<Option<RuntimeConfig>> {
    let store = EspNvs::new(nvs.clone(), NS, true)?;
    if store.get_u8(KEY_PROVISIONED)?.unwrap_or(0) != 1 {
        return Ok(None);
    }
    let mut buf = vec![0u8; MAX_BLOB];
    let Some(bytes) = store.get_blob(KEY_PAYLOAD, &mut buf)? else {
        return Ok(None);
    };
    let payload: Payload = serde_json::from_slice(bytes)?;
    let runtime = payload_to_runtime(&payload).map_err(|e| anyhow!(e))?;
    Ok(Some(runtime))
}

/// Clear the provisioning record so the next boot re-enters provisioning mode.
pub fn clear(nvs: &EspDefaultNvsPartition) -> anyhow::Result<()> {
    let mut store = EspNvs::new(nvs.clone(), NS, true)?;
    store.remove(KEY_PROVISIONED)?;
    store.remove(KEY_PAYLOAD)?;
    Ok(())
}

/// Build a RuntimeConfig from the baked `cfg.toml`, or `None` if it is
/// incomplete (so an un-flashed/empty board falls through to provisioning).
pub fn from_toml(config: &Config) -> Option<RuntimeConfig> {
    let networks: Vec<(String, String)> = [
        (config.wifi_ssid, config.wifi_psk),
        (config.wifi_ssid2, config.wifi_psk2),
        (config.wifi_ssid3, config.wifi_psk3),
    ]
    .into_iter()
    .filter(|(ssid, _)| !ssid.is_empty())
    .map(|(ssid, psk)| (ssid.to_string(), psk.to_string()))
    .collect();
    let control_key = parse_key(config.control_key)?;
    if networks.is_empty() || config.hub_addr.is_empty() {
        return None;
    }
    Some(RuntimeConfig {
        networks,
        hub_addr: config.hub_addr.to_string(),
        control_key,
        audio_addr: config.audio_addr.to_string(),
        wake_db_threshold: config.wake_db_threshold,
        silence_timeout_ms: config.silence_timeout_ms,
        led_gpio: config.led_gpio,
    })
}
