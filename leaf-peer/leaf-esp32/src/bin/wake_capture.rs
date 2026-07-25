//! Temporary, detector-independent wake-word corpus recorder.
//!
//! Tap BOOT for each sample. The Leaf gives a short piezo cue, waits for the cue
//! to decay, lights yellow, records 1.8 seconds from the normal microphone path,
//! and sends PCM16LE to the host collector. This binary deliberately bypasses
//! microWakeWord so missed wakes are represented in the training corpus.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::anyhow;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay::{BLOCK, FreeRtos};
use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, OutputPin};
use esp_idf_svc::hal::i2s::config::{
    Config as I2sConfig, DataBitWidth, SlotMode, StdClkConfig, StdConfig, StdGpioConfig,
    StdSlotConfig,
};
use esp_idf_svc::hal::i2s::{I2sDriver, I2sRx};
use esp_idf_svc::hal::ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver};
use esp_idf_svc::hal::prelude::*;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::{info, warn};

#[path = "../led.rs"]
mod led;

const SAMPLE_RATE: u32 = 16_000;
const CLIP_MS: usize = 1_800;
const CLIP_SAMPLES: usize = SAMPLE_RATE as usize * CLIP_MS / 1_000;
const READ_BYTES: usize = 4_096;
const TARGET_SAMPLES: u32 = 60;
const CAPTURE_HOST: &str = "192.168.1.71:10001";

#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
    #[default("")]
    wifi_ssid2: &'static str,
    #[default("")]
    wifi_psk2: &'static str,
    #[default("")]
    wifi_ssid3: &'static str,
    #[default("")]
    wifi_psk3: &'static str,
    #[default(3)]
    mic_gain_shift: i32,
    #[default(48)]
    led_gpio: i32,
}

fn connect_wifi(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    networks: &[(&'static str, &'static str)],
) -> anyhow::Result<()> {
    let access_points = wifi.scan().unwrap_or_default();
    let mut candidates: Vec<(usize, i8)> = networks
        .iter()
        .enumerate()
        .filter_map(|(index, (ssid, _))| {
            access_points
                .iter()
                .filter(|ap| ap.ssid == *ssid)
                .map(|ap| ap.signal_strength)
                .max()
                .map(|rssi| (index, rssi))
        })
        .collect();
    candidates.sort_by_key(|(_, rssi)| std::cmp::Reverse(*rssi));
    let order: Vec<usize> = if candidates.is_empty() {
        (0..networks.len()).collect()
    } else {
        candidates.into_iter().map(|(index, _)| index).collect()
    };

    for index in order {
        let (ssid, psk) = networks[index];
        info!("[capture] connecting to wifi '{ssid}'");
        let client = ClientConfiguration {
            ssid: ssid.try_into().map_err(|_| anyhow!("ssid too long"))?,
            password: psk.try_into().map_err(|_| anyhow!("password too long"))?,
            auth_method: if psk.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
            ..Default::default()
        };
        if wifi.set_configuration(&Configuration::Client(client)).is_err() {
            continue;
        }
        if wifi.connect().and_then(|()| wifi.wait_netif_up()).is_ok() {
            info!("[capture] wifi ready: {:?}", wifi.wifi().sta_netif().get_ip_info()?);
            return Ok(());
        }
        let _ = wifi.disconnect();
    }
    Err(anyhow!("no configured wifi network was reachable"))
}

fn connect_collector() -> TcpStream {
    loop {
        match TcpStream::connect(CAPTURE_HOST) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
                let _ = stream.set_nodelay(true);
                info!("[capture] collector connected at {CAPTURE_HOST}");
                return stream;
            }
            Err(error) => {
                warn!("[capture] collector unavailable at {CAPTURE_HOST}: {error}; retrying");
                FreeRtos::delay_ms(1_000);
            }
        }
    }
}

fn beep(buzzer: &mut LedcDriver<'_>, half_duty: u32) -> anyhow::Result<()> {
    esp_idf_svc::sys::esp!(unsafe {
        esp_idf_svc::sys::ledc_set_freq(
            esp_idf_svc::sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
            esp_idf_svc::sys::ledc_timer_t_LEDC_TIMER_0,
            1_047,
        )
    })?;
    buzzer.set_duty(half_duty)?;
    FreeRtos::delay_ms(70);
    buzzer.set_duty(0)?;
    Ok(())
}

fn capture_clip(mic: &mut I2sDriver<'_, I2sRx>, pcm_shift: u32) -> anyhow::Result<Vec<i16>> {
    let mut pcm = Vec::with_capacity(CLIP_SAMPLES);
    let mut buf = [0u8; READ_BYTES];
    mic.rx_enable()?;
    while pcm.len() < CLIP_SAMPLES {
        let bytes = mic.read(&mut buf, BLOCK)?;
        for frame in buf[..bytes].chunks_exact(4) {
            let raw = i32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
            pcm.push((raw >> pcm_shift).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            if pcm.len() == CLIP_SAMPLES {
                break;
            }
        }
    }
    mic.rx_disable()?;
    Ok(pcm)
}

fn send_clip(stream: &mut TcpStream, sequence: u32, pcm: &[i16]) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut header = [0u8; 16];
    header[..4].copy_from_slice(b"PET1");
    header[4..8].copy_from_slice(&sequence.to_le_bytes());
    header[8..12].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    header[12..16].copy_from_slice(&(pcm.len() as u32).to_le_bytes());
    stream.write_all(&header)?;
    let pcm_bytes = unsafe {
        std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), pcm.len() * std::mem::size_of::<i16>())
    };
    stream.write_all(pcm_bytes)?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack)?;
    if ack != [0x06] {
        return Err(anyhow!("collector returned invalid acknowledgement"));
    }
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let config = CONFIG;
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    unsafe {
        esp_idf_svc::sys::esp_bt_mem_release(esp_idf_svc::sys::esp_bt_mode_t_ESP_BT_MODE_BTDM);
    }

    let led_pin: AnyOutputPin = if config.led_gpio == 38 {
        peripherals.pins.gpio38.downgrade_output()
    } else {
        peripherals.pins.gpio48.downgrade_output()
    };
    let mut led = led::Led::new(peripherals.rmt.channel0, led_pin)?;
    let _ = led.blue();

    let timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new().frequency(1.kHz().into()),
    )?;
    let mut buzzer = LedcDriver::new(
        peripherals.ledc.channel0,
        timer,
        peripherals.pins.gpio7,
    )?;
    let half_duty = buzzer.get_max_duty() / 2;
    buzzer.set_duty(0)?;

    let mic_config = StdConfig::new(
        I2sConfig::default(),
        StdClkConfig::from_sample_rate_hz(SAMPLE_RATE),
        StdSlotConfig::philips_slot_default(DataBitWidth::Bits32, SlotMode::Mono),
        StdGpioConfig::default(),
    );
    let mut mic = I2sDriver::new_std_rx(
        peripherals.i2s0,
        &mic_config,
        peripherals.pins.gpio4,
        peripherals.pins.gpio6,
        None::<AnyIOPin>,
        peripherals.pins.gpio5,
    )?;

    let networks: Vec<(&'static str, &'static str)> = [
        (config.wifi_ssid, config.wifi_psk),
        (config.wifi_ssid2, config.wifi_psk2),
        (config.wifi_ssid3, config.wifi_psk3),
    ]
    .into_iter()
    .filter(|(ssid, _)| !ssid.is_empty())
    .collect();
    if networks.is_empty() {
        return Err(anyhow!("wake capture requires wifi credentials in cfg.toml"));
    }
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;
    wifi.start()?;
    connect_wifi(&mut wifi, &networks)?;
    let mut collector = connect_collector();
    // Preserve the microphone's unclipped signal in the corpus. The normal
    // firmware's configured +18 dB path (shift=3) clipped heavily during the
    // calibration samples; augmentation/normalization belongs in training.
    let pcm_shift = 16;
    info!(
        "[capture] recording raw PCM (gain shift 0; normal configured shift {} is intentionally bypassed)",
        config.mic_gain_shift
    );

    let mut sequence = 0u32;
    let _ = led.green();
    info!("[capture] READY: waiting for host trigger, then beep/yellow means say petito ({TARGET_SAMPLES} samples)");
    while sequence < TARGET_SAMPLES {
        let mut remotely_triggered = false;
        let mut control = [0u8; 1];
        match collector.read(&mut control) {
            Ok(1) if control[0] == b'R' => remotely_triggered = true,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                warn!("[capture] control connection lost: {error}; reconnecting");
                let _ = led.red();
                collector = connect_collector();
                let _ = led.green();
                continue;
            }
        }

        if !remotely_triggered {
            continue;
        }

        let _ = led.blue();
        beep(&mut buzzer, half_duty)?;
        FreeRtos::delay_ms(300);
        let _ = led.yellow();
        info!("[capture] RECORDING sample {}/{} — say petito", sequence + 1, TARGET_SAMPLES);
        let pcm = capture_clip(&mut mic, pcm_shift)?;
        let _ = led.purple();

        loop {
            match send_clip(&mut collector, sequence + 1, &pcm) {
                Ok(()) => break,
                Err(error) => {
                    warn!("[capture] send failed: {error:#}; reconnecting without losing sample");
                    let _ = led.red();
                    collector = connect_collector();
                }
            }
        }
        sequence += 1;
        let _ = led.green();
        info!("[capture] SAVED {sequence}/{TARGET_SAMPLES}; waiting for the next trigger");
        FreeRtos::delay_ms(250);
    }

    let _ = led.blue();
    info!("[capture] COMPLETE: {TARGET_SAMPLES}/{TARGET_SAMPLES} petito clips saved");
    loop {
        FreeRtos::delay_ms(1_000);
    }
}
