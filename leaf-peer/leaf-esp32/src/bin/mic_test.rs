//! Standalone INMP441 I2S microphone bring-up test for the listam leaf (ESP32-S3).
//!
//! Flashes a tiny level-meter: it reads the mic and prints a per-window peak/RMS
//! bar to the serial monitor, so you can literally watch the bar grow when you
//! talk or tap the mic. It touches no storage/wifi/FAT — pure mic bring-up.
//!
//! Wiring (INMP441 -> ESP32-S3, L/R tied to GND = left slot):
//!   VDD -> 3V3    GND -> GND    L/R -> GND
//!   SCK (BCLK) -> GPIO4    WS (LRCL) -> GPIO5    SD (data out) -> GPIO6
//!
//! Build + flash + monitor (from the crate dir, with the esp env sourced):
//!   . ~/export-esp.sh
//!   cargo run --release --bin mic_test
//!
//! This is an isolated extra binary. Delete this file to remove the test; it
//! does not affect the `leaf-esp32` firmware.

use esp_idf_svc::hal::delay::BLOCK;
use esp_idf_svc::hal::gpio::AnyIOPin;
use esp_idf_svc::hal::i2s::config::{
    Config, DataBitWidth, SlotMode, StdClkConfig, StdConfig, StdGpioConfig, StdSlotConfig,
};
use esp_idf_svc::hal::i2s::I2sDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::log::EspLogger;

const SAMPLE_RATE: u32 = 16_000;
// One 32-bit slot word per mono frame; a ~100 ms window paces the printout to ~10 Hz.
const FRAMES_PER_WINDOW: usize = SAMPLE_RATE as usize / 10;
const BYTES_PER_WINDOW: usize = FRAMES_PER_WINDOW * 4;
const FULL_SCALE_24BIT: f64 = 8_388_608.0; // 2^23 — a 24-bit sample's magnitude limit

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;

    // INMP441: Philips standard I2S, 24-bit data left-justified in a 32-bit slot,
    // left channel only (L/R = GND). The Bits32 slot + Mono/Left pairing is the
    // config that avoids the espressif/esp-idf #15770 "reads 0 or garbage" bug.
    let cfg = StdConfig::new(
        Config::default(),
        StdClkConfig::from_sample_rate_hz(SAMPLE_RATE),
        StdSlotConfig::philips_slot_default(DataBitWidth::Bits32, SlotMode::Mono),
        StdGpioConfig::default(),
    );

    // new_std_rx(i2s, &config, bclk, din, mclk, ws). INMP441 has no MCLK -> None.
    let mut mic = I2sDriver::new_std_rx(
        peripherals.i2s0,
        &cfg,
        peripherals.pins.gpio4, // BCLK / SCK
        peripherals.pins.gpio6, // DIN  <- mic SD (data out)
        None::<AnyIOPin>,       // no MCLK on the INMP441
        peripherals.pins.gpio5, // WS / LRCL
    )?;
    mic.rx_enable()?;

    log::info!("INMP441 mic test up: {SAMPLE_RATE} Hz, 24-in-32 left slot, BCLK=4 WS=5 DIN=6");
    log::info!("Quiet room reads a small nonzero floor; tapping/talking should swing it hard.");

    let mut buf = [0u8; BYTES_PER_WINDOW];
    loop {
        // Blocking read fills the whole window (~100 ms of audio), pacing the loop.
        let n = mic.read(&mut buf, BLOCK)?;
        let frames = n / 4;
        if frames == 0 {
            continue;
        }

        let mut peak: i32 = 0;
        let mut min: i32 = i32::MAX;
        let mut max: i32 = i32::MIN;
        let mut sum_sq: i64 = 0;
        for f in 0..frames {
            let o = f * 4;
            // The 24-bit sample sits in the top 24 bits of the 32-bit slot word.
            let sample = i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) >> 8;
            let mag = sample.abs();
            if mag > peak {
                peak = mag;
            }
            if sample < min {
                min = sample;
            }
            if sample > max {
                max = sample;
            }
            sum_sq += (sample as i64) * (sample as i64);
        }

        let rms = ((sum_sq / frames as i64) as f64).sqrt();
        let dbfs = if peak > 0 {
            20.0 * (peak as f64 / FULL_SCALE_24BIT).log10()
        } else {
            -120.0
        };
        // Map -60..0 dBFS onto a 0..40 char bar so quiet speech is still visible.
        let bar_len = (((dbfs + 60.0) / 60.0) * 40.0).clamp(0.0, 40.0) as usize;
        let bar = "#".repeat(bar_len);
        let pad = " ".repeat(40 - bar_len);
        log::info!(
            "[{bar}{pad}] peak={peak:8}  rms={rms:8.0}  min={min:8}  max={max:8}  {dbfs:6.1} dBFS"
        );
    }
}
