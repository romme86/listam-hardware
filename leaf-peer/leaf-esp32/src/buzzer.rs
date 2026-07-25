//! GPIO7 passive-piezo square-wave player shared by the normal voice firmware
//! and the standalone tune demo.

use anyhow::Result;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::Gpio7;
use esp_idf_svc::hal::ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver, CHANNEL0, TIMER0};
use esp_idf_svc::hal::prelude::*;

use crate::tunes::{Note, Tune, GAME_TUNES};

pub struct Buzzer<'d> {
    driver: LedcDriver<'d>,
    half_duty: u32,
    last_tune: Option<usize>,
}

impl<'d> Buzzer<'d> {
    pub fn new(timer: TIMER0, channel: CHANNEL0, pin: Gpio7) -> Result<Self> {
        let timer = LedcTimerDriver::new(timer, &TimerConfig::new().frequency(1.kHz().into()))?;
        let mut driver = LedcDriver::new(channel, timer, pin)?;
        driver.set_duty(0)?;
        let half_duty = driver.get_max_duty() / 2;
        Ok(Self {
            driver,
            half_duty,
            last_tune: None,
        })
    }

    fn set_frequency(frequency: u32) -> Result<()> {
        esp_idf_svc::sys::esp!(unsafe {
            esp_idf_svc::sys::ledc_set_freq(
                esp_idf_svc::sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
                esp_idf_svc::sys::ledc_timer_t_LEDC_TIMER_0,
                frequency,
            )
        })?;
        Ok(())
    }

    fn play_note(&mut self, note: Note) -> Result<()> {
        if note.frequency_hz == 0 {
            self.driver.set_duty(0)?;
            FreeRtos::delay_ms(note.duration_ms);
            return Ok(());
        }

        Self::set_frequency(note.frequency_hz)?;
        self.driver.set_duty(self.half_duty)?;
        let sounding_ms = note.duration_ms * 85 / 100;
        FreeRtos::delay_ms(sounding_ms);
        self.driver.set_duty(0)?;
        FreeRtos::delay_ms(note.duration_ms - sounding_ms);
        Ok(())
    }

    pub fn play(&mut self, tune: Tune) -> Result<()> {
        self.driver.set_duty(0)?;
        for &note in tune.notes {
            self.play_note(note)?;
        }
        self.driver.set_duty(0)?;
        Ok(())
    }

    pub fn play_random(&mut self) -> Result<Tune> {
        let mut index = unsafe { esp_idf_svc::sys::esp_random() as usize } % GAME_TUNES.len();
        if GAME_TUNES.len() > 1 && self.last_tune == Some(index) {
            index = (index + 1) % GAME_TUNES.len();
        }
        self.last_tune = Some(index);
        let tune = GAME_TUNES[index];
        self.play(tune)?;
        Ok(tune)
    }
}
