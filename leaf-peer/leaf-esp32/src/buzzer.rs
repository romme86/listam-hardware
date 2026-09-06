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
    tune_order: [usize; GAME_TUNES.len()],
    next_tune: usize,
    last_tune: Option<usize>,
}

impl<'d> Buzzer<'d> {
    pub fn new(timer: TIMER0, channel: CHANNEL0, pin: Gpio7) -> Result<Self> {
        let timer = LedcTimerDriver::new(timer, &TimerConfig::new().frequency(1.kHz().into()))?;
        let mut driver = LedcDriver::new(channel, timer, pin)?;
        driver.set_duty(0)?;
        let half_duty = driver.get_max_duty() / 2;
        let mut tune_order = [0; GAME_TUNES.len()];
        for (index, slot) in tune_order.iter_mut().enumerate() {
            *slot = index;
        }
        Self::shuffle(&mut tune_order, None);

        Ok(Self {
            driver,
            half_duty,
            tune_order,
            next_tune: 0,
            last_tune: None,
        })
    }

    fn shuffle(order: &mut [usize], avoid_first: Option<usize>) {
        for index in (1..order.len()).rev() {
            let swap_with = unsafe { esp_idf_svc::sys::esp_random() as usize } % (index + 1);
            order.swap(index, swap_with);
        }

        // A new cycle may not begin with the tune that ended the previous one.
        if order.len() > 1 && avoid_first == Some(order[0]) {
            order.swap(0, 1);
        }
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
        if self.next_tune == self.tune_order.len() {
            Self::shuffle(&mut self.tune_order, self.last_tune);
            self.next_tune = 0;
        }

        let index = self.tune_order[self.next_tune];
        let tune = GAME_TUNES[index];
        self.play(tune)?;
        self.next_tune += 1;
        self.last_tune = Some(index);
        Ok(tune)
    }
}
