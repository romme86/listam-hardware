//! Standalone GPIO7 passive-piezo test for the Listam Leaf.
//!
//! Wiring:
//!   GPIO7 -> red buzzer lead
//!   GND   -> black buzzer lead
//!
//! This deliberately does not start Wi-Fi, the microphone, storage, or the
//! normal Leaf runtime. It loops a short 8-bit platform-theme excerpt so the
//! buzzer wiring and pitch generation can be checked on real hardware.

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver};
use esp_idf_svc::hal::prelude::Peripherals;
use esp_idf_svc::hal::prelude::*;

const REST: u32 = 0;

// Frequencies used by the familiar opening and first phrase. The piezo's
// resonant peak is near 4 kHz, so the original upper register is a good fit.
const E7: u32 = 2637;
const C7: u32 = 2093;
const G7: u32 = 3136;
const G6: u32 = 1568;
const E6: u32 = 1319;
const A6: u32 = 1760;
const B6: u32 = 1976;
const AS6: u32 = 1865;
const F7: u32 = 2794;
const A7: u32 = 3520;
const D7: u32 = 2349;

// (frequency in Hz, total step duration in ms). A short silence is inserted
// between adjacent notes so repeated pitches remain crisp.
const MELODY: &[(u32, u32)] = &[
    (E7, 150),
    (E7, 150),
    (REST, 150),
    (E7, 150),
    (REST, 150),
    (C7, 150),
    (E7, 150),
    (REST, 150),
    (G7, 150),
    (REST, 450),
    (G6, 150),
    (REST, 450),
    (C7, 150),
    (REST, 300),
    (G6, 150),
    (REST, 300),
    (E6, 150),
    (REST, 300),
    (A6, 150),
    (B6, 150),
    (AS6, 150),
    (A6, 150),
    (G6, 100),
    (E7, 100),
    (G7, 100),
    (A7, 150),
    (F7, 150),
    (G7, 150),
    (REST, 150),
    (E7, 150),
    (C7, 150),
    (D7, 150),
    (B6, 150),
    (REST, 300),
];

fn set_frequency(frequency: u32) -> anyhow::Result<()> {
    esp_idf_svc::sys::esp!(unsafe {
        esp_idf_svc::sys::ledc_set_freq(
            esp_idf_svc::sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
            esp_idf_svc::sys::ledc_timer_t_LEDC_TIMER_0,
            frequency,
        )
    })?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new().frequency(1.kHz().into()),
    )?;
    let mut buzzer = LedcDriver::new(peripherals.ledc.channel0, timer, peripherals.pins.gpio7)?;
    let half_duty = buzzer.get_max_duty() / 2;

    log::info!("passive buzzer test ready: GPIO7, 50% square wave");
    FreeRtos::delay_ms(1000);

    loop {
        log::info!("playing melody test");
        for &(frequency, duration_ms) in MELODY {
            if frequency == REST {
                buzzer.set_duty(0)?;
                FreeRtos::delay_ms(duration_ms);
                continue;
            }

            set_frequency(frequency)?;
            buzzer.set_duty(half_duty)?;

            // Sound for 85% of the step and leave a small articulation gap.
            let sounding_ms = duration_ms * 85 / 100;
            FreeRtos::delay_ms(sounding_ms);
            buzzer.set_duty(0)?;
            FreeRtos::delay_ms(duration_ms - sounding_ms);
        }

        buzzer.set_duty(0)?;
        log::info!("melody complete; repeating in 2 seconds");
        FreeRtos::delay_ms(2000);
    }
}
