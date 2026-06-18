//! BLE provisioning mode: advertise a tiny custom GATT service (via the
//! `components/leaf_prov` NimBLE C shim), wait for a listam app to write the
//! config payload, persist it to NVS, and reboot into the normal path.
//!
//! All BLE + framing + CRC live in the C shim; this Rust side owns the JSON
//! decode/validation/NVS write and the status/LED feedback. The shim hands up
//! only CRC-verified payloads, so a corrupt transfer never reaches here.

use std::ffi::CString;

use anyhow::{anyhow, Context};
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::leaf_prov;
use log::{error, info, warn};

use crate::config::{self, ProvError};
use crate::led::Led;

// Status codes mirror @listam/provisioning STATUS (and leaf_prov.h).
const ST_APPLYING: u8 = 2;
const ST_OK: u8 = 3;
const ST_ERR_DECODE: u8 = 5;
const ST_ERR_VALIDATE: u8 = 6;
const ST_ERR_NVS: u8 = 7;

/// Run provisioning until a valid payload is received (then reboot). Returns
/// only on an unrecoverable BLE-startup error.
pub fn run(nvs: &EspDefaultNvsPartition, led: &mut Led, device_name: &str) -> anyhow::Result<()> {
    let cname = CString::new(device_name).context("device name has interior NUL")?;
    let rc = unsafe { leaf_prov::leaf_prov_start(cname.as_ptr()) };
    if rc != 0 {
        return Err(anyhow!("leaf_prov_start failed (rc={rc})"));
    }
    info!("[provision] advertising as '{device_name}' — waiting for a listam app…");
    let _ = led.blue();

    let mut buf = vec![0u8; 1024];
    loop {
        let n = unsafe {
            leaf_prov::leaf_prov_wait_payload(buf.as_mut_ptr(), buf.len() as core::ffi::c_int, 30_000)
        };
        if n == 0 {
            // Idle timeout: still advertising. Blink once to show liveness.
            let _ = led.off();
            FreeRtos::delay_ms(120);
            let _ = led.blue();
            continue;
        }
        if n < 0 {
            warn!("[provision] wait_payload error ({n})");
            continue;
        }

        let payload = &buf[..n as usize];
        unsafe { leaf_prov::leaf_prov_notify_status(ST_APPLYING) };
        match config::store_payload(nvs, payload) {
            Ok(fingerprint) => {
                info!("[provision] config stored (control_key {fingerprint}…); rebooting into normal mode");
                unsafe { leaf_prov::leaf_prov_notify_status(ST_OK) };
                let _ = led.green();
                // Let the OK notification flush before tearing BLE down + restarting,
                // which fully reclaims the controller RAM for the WiFi/leaf path.
                FreeRtos::delay_ms(500);
                unsafe { leaf_prov::leaf_prov_stop() };
                FreeRtos::delay_ms(100);
                unsafe { esp_idf_svc::sys::esp_restart() };
            }
            Err(err) => {
                let code = match &err {
                    ProvError::Decode(msg) => {
                        warn!("[provision] payload decode failed: {msg}");
                        ST_ERR_DECODE
                    }
                    ProvError::Validate(msg) => {
                        warn!("[provision] payload invalid: {msg}");
                        ST_ERR_VALIDATE
                    }
                    ProvError::Nvs(msg) => {
                        error!("[provision] NVS write failed: {msg}");
                        ST_ERR_NVS
                    }
                };
                unsafe { leaf_prov::leaf_prov_notify_status(code) };
                let _ = led.red();
                // Stay in provisioning mode so the app can correct and retry.
            }
        }
    }
}
