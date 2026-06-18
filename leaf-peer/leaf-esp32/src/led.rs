//! Onboard addressable RGB LED (WS2812/SK68xx) status indicator, driven over the
//! RMT peripheral. The DevKitC-1's single LED is on GPIO48 (v1.0) or GPIO38
//! (v1.1) — selected by `led_gpio` in cfg.toml. Used by the voice thread, but
//! the colors are driven by the HOST over the voice socket (not the local dB
//! gate): off = idle/capturing, yellow = wake word recognized, purple = command
//! recognized, green = saved, red = error.

use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::hal::gpio::AnyOutputPin;
use esp_idf_svc::hal::rmt::{
    config::TransmitConfig, FixedLengthSignal, PinState, Pulse, RmtChannel, TxRmtDriver,
};
use esp_idf_svc::hal::peripheral::Peripheral;

/// WS2812 bit timings (ns): a 0 bit is a short high, a 1 bit a long high.
const T0H_NS: u64 = 350;
const T0L_NS: u64 = 800;
const T1H_NS: u64 = 700;
const T1L_NS: u64 = 600;

pub struct Led<'d> {
    tx: TxRmtDriver<'d>,
    t0h: Pulse,
    t0l: Pulse,
    t1h: Pulse,
    t1l: Pulse,
}

impl<'d> Led<'d> {
    pub fn new(
        channel: impl Peripheral<P = impl RmtChannel> + 'd,
        pin: AnyOutputPin,
    ) -> Result<Self> {
        let config = TransmitConfig::new().clock_divider(1);
        let tx = TxRmtDriver::new(channel, pin, &config)?;
        let ticks_hz = tx.counter_clock()?;
        let mk = |state: PinState, ns: u64| Pulse::new_with_duration(ticks_hz, state, &Duration::from_nanos(ns));
        let led = Led {
            t0h: mk(PinState::High, T0H_NS)?,
            t0l: mk(PinState::Low, T0L_NS)?,
            t1h: mk(PinState::High, T1H_NS)?,
            t1l: mk(PinState::Low, T1L_NS)?,
            tx,
        };
        Ok(led)
    }

    /// Write one pixel. Bytes are in WS2812 GRB order.
    fn write_grb(&mut self, g: u8, r: u8, b: u8) -> Result<()> {
        let bits = ((g as u32) << 16) | ((r as u32) << 8) | (b as u32);
        let mut signal = FixedLengthSignal::<24>::new();
        for i in 0..24 {
            let bit = (bits >> (23 - i)) & 1;
            let (high, low) = if bit == 1 { (self.t1h, self.t1l) } else { (self.t0h, self.t0l) };
            signal.set(i, &(high, low))?;
        }
        self.tx.start_blocking(&signal)?;
        Ok(())
    }

    pub fn off(&mut self) -> Result<()> {
        self.write_grb(0, 0, 0)
    }

    /// Yellow = red + green (dimmed so it isn't blinding).
    pub fn yellow(&mut self) -> Result<()> {
        self.write_grb(40, 40, 0)
    }

    /// Purple = red + blue.
    pub fn purple(&mut self) -> Result<()> {
        self.write_grb(0, 40, 40)
    }

    pub fn green(&mut self) -> Result<()> {
        self.write_grb(48, 0, 0)
    }

    /// Red = error / command not understood.
    pub fn red(&mut self) -> Result<()> {
        self.write_grb(0, 48, 0)
    }

    /// Blue = BLE provisioning mode (advertising, waiting for an app to pair).
    pub fn blue(&mut self) -> Result<()> {
        self.write_grb(0, 0, 48)
    }
}
