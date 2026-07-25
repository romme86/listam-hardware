//! Standalone five-tune GPIO7 piezo player.
//!
//! Flash this binary only for a music demo. The normal Leaf firmware remains
//! the default build. Tap BOOT/GPIO0 to play the named tune shown in the serial
//! log; each completed playback advances to the next tune.

#[path = "../tunes.rs"]
mod tunes;

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::{PinDriver, Pull};
use esp_idf_svc::hal::ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver};
use esp_idf_svc::hal::prelude::Peripherals;
use esp_idf_svc::hal::prelude::*;
use tunes::{Note, Tune, GAME_TUNES};

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

fn play_note(buzzer: &mut LedcDriver<'_>, note: Note, half_duty: u32) -> anyhow::Result<()> {
    if note.frequency_hz == 0 {
        buzzer.set_duty(0)?;
        FreeRtos::delay_ms(note.duration_ms);
        return Ok(());
    }

    set_frequency(note.frequency_hz)?;
    buzzer.set_duty(half_duty)?;
    let sounding_ms = note.duration_ms * 85 / 100;
    FreeRtos::delay_ms(sounding_ms);
    buzzer.set_duty(0)?;
    FreeRtos::delay_ms(note.duration_ms - sounding_ms);
    Ok(())
}

fn play_tune(buzzer: &mut LedcDriver<'_>, tune: Tune, half_duty: u32) -> anyhow::Result<()> {
    log::info!("playing {}", tune.title);
    for &note in tune.notes {
        play_note(buzzer, note, half_duty)?;
    }
    buzzer.set_duty(0)?;
    log::info!("finished {}", tune.title);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let mut button = PinDriver::input(peripherals.pins.gpio0)?;
    button.set_pull(Pull::Up)?;

    let timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new().frequency(1.kHz().into()),
    )?;
    let mut buzzer = LedcDriver::new(peripherals.ledc.channel0, timer, peripherals.pins.gpio7)?;
    let half_duty = buzzer.get_max_duty() / 2;
    let mut selected = 0usize;

    log::info!(
        "game-tune player ready; tap BOOT to play {}",
        GAME_TUNES[selected].title
    );
    loop {
        if button.is_low() {
            FreeRtos::delay_ms(30);
            if button.is_low() {
                while button.is_low() {
                    FreeRtos::delay_ms(10);
                }
                play_tune(&mut buzzer, GAME_TUNES[selected], half_duty)?;
                selected = (selected + 1) % GAME_TUNES.len();
                log::info!("tap BOOT to play {}", GAME_TUNES[selected].title);
            }
        }
        FreeRtos::delay_ms(20);
    }
}
