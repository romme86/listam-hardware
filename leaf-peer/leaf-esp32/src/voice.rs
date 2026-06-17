//! Voice front-end thread: capture mic audio, gate on loudness, and stream an
//! utterance to the paired host for transcription, with RGB-LED feedback.
//!
//! v1 wake trigger is the loudness (dB) gate — a deliberate, loud sound opens an
//! utterance. The custom microWakeWord models ("yo" / "hey listam" /
//! "dai dai dai dai") plug in where `is_wake()` is computed (Task 8); the dB gate
//! stays in front of "yo" regardless. Reuses the verified mic_test I2S setup.
//!
//! Wire frames to the host audio bridge: [u24le bodyLen][type][payload]
//!   0x00 hello(utf8 id) · 0x01 start(wakeWordId:u8, epochMs:u32le)
//!   0x02 chunk(PCM16LE 16k mono) · 0x03 end(reason:u8 0=silence/1=max)

use std::collections::VecDeque;
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use esp_idf_svc::hal::delay::BLOCK;
use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Gpio4, Gpio5, Gpio6};
use esp_idf_svc::hal::i2s::config::{
    Config, DataBitWidth, SlotMode, StdClkConfig, StdConfig, StdGpioConfig, StdSlotConfig,
};
use esp_idf_svc::hal::i2s::{I2sDriver, I2S0};
use esp_idf_svc::hal::rmt::CHANNEL0;
use log::{info, warn};

use crate::led::Led;

const SAMPLE_RATE: u32 = 16_000;
const FULL_SCALE_24BIT: f32 = 8_388_608.0;
const READ_BYTES: usize = 4096; // 1024 frames (32-bit slot) ~= 64 ms @ 16 kHz
const MAX_UTTERANCE_MS: u32 = 12_000;
const PREROLL_WINDOWS: usize = 4; // ~256 ms kept before the wake fires

const F_HELLO: u8 = 0x00;
const F_START: u8 = 0x01;
const F_CHUNK: u8 = 0x02;
const F_END: u8 = 0x03;

fn send_frame(stream: &mut TcpStream, ftype: u8, payload: &[u8]) -> std::io::Result<()> {
    let body_len = 1 + payload.len();
    let hdr = [body_len as u8, (body_len >> 8) as u8, (body_len >> 16) as u8, ftype];
    stream.write_all(&hdr)?;
    stream.write_all(payload)
}

pub fn run(
    i2s0: I2S0,
    bclk: Gpio4,
    din: Gpio6,
    ws: Gpio5,
    rmt: CHANNEL0,
    led_pin: AnyOutputPin,
    audio_addr: &'static str,
    wake_db_threshold: f32,
    silence_timeout_ms: u32,
) {
    let mut led = match Led::new(rmt, led_pin) {
        Ok(l) => l,
        Err(err) => { warn!("[voice] LED init failed ({err:#}); continuing without it"); return; }
    };
    let _ = led.off();

    let cfg = StdConfig::new(
        Config::default(),
        StdClkConfig::from_sample_rate_hz(SAMPLE_RATE),
        StdSlotConfig::philips_slot_default(DataBitWidth::Bits32, SlotMode::Mono),
        StdGpioConfig::default(),
    );
    let mut mic = match I2sDriver::new_std_rx(i2s0, &cfg, bclk, din, None::<AnyIOPin>, ws) {
        Ok(m) => m,
        Err(err) => { warn!("[voice] I2S init failed: {err:#}"); return; }
    };
    if let Err(err) = mic.rx_enable() {
        warn!("[voice] I2S rx_enable failed: {err:#}");
        return;
    }
    info!("[voice] up: wake>={wake_db_threshold:.0} dBFS, silence {silence_timeout_ms} ms, host {audio_addr}");

    let mut buf = [0u8; READ_BYTES];
    let mut preroll: VecDeque<Vec<u8>> = VecDeque::new();
    let mut stream: Option<TcpStream> = None;
    let mut silent_ms = 0u32;
    let mut active_ms = 0u32;
    let mut blink = 0u32;

    loop {
        let n = mic.read(&mut buf, BLOCK).unwrap_or(0);
        let frames = n / 4;
        if frames == 0 { continue; }
        let window_ms = (frames as u32 * 1000) / SAMPLE_RATE;

        // One pass: peak (24-bit, for dBFS) + PCM16 (top 16 bits of each slot word).
        let mut peak: i32 = 0;
        let mut pcm = Vec::with_capacity(frames * 2);
        for f in 0..frames {
            let o = f * 4;
            let raw = i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
            let mag = (raw >> 8).abs();
            if mag > peak { peak = mag; }
            pcm.extend_from_slice(&((raw >> 16) as i16).to_le_bytes());
        }
        let dbfs = if peak > 0 { 20.0 * (peak as f32 / FULL_SCALE_24BIT).log10() } else { -120.0 };

        match stream {
            None => {
                preroll.push_back(pcm);
                while preroll.len() > PREROLL_WINDOWS { preroll.pop_front(); }
                if dbfs >= wake_db_threshold {
                    match TcpStream::connect(audio_addr) {
                        Ok(mut s) => {
                            s.set_nodelay(true).ok();
                            let _ = send_frame(&mut s, F_HELLO, b"leaf");
                            let _ = send_frame(&mut s, F_START, &[1, 0, 0, 0, 0]); // wakeWordId=1 (dB gate)
                            for p in preroll.drain(..) { let _ = send_frame(&mut s, F_CHUNK, &p); }
                            let _ = led.yellow();
                            blink = 0; silent_ms = 0; active_ms = 0;
                            stream = Some(s);
                            info!("[voice] wake ({dbfs:.1} dBFS) -> streaming");
                        }
                        Err(err) => { warn!("[voice] connect {audio_addr} failed: {err}"); preroll.clear(); }
                    }
                }
            }
            Some(ref mut s) => {
                if send_frame(s, F_CHUNK, &pcm).is_err() {
                    warn!("[voice] stream dropped");
                    let _ = led.off();
                    stream = None;
                    continue;
                }
                active_ms += window_ms;
                if dbfs < wake_db_threshold - 10.0 { silent_ms += window_ms; } else { silent_ms = 0; }

                // Blink yellow ~every 320 ms while listening.
                blink += 1;
                let _ = if (blink / 5) % 2 == 0 { led.yellow() } else { led.off() };

                if silent_ms >= silence_timeout_ms || active_ms >= MAX_UTTERANCE_MS {
                    let reason = if active_ms >= MAX_UTTERANCE_MS { 1u8 } else { 0u8 };
                    let _ = send_frame(s, F_END, &[reason]);
                    let _ = s.flush();
                    let _ = led.green(); // "sending"
                    std::thread::sleep(Duration::from_millis(350));
                    let _ = led.off();
                    let _ = s.shutdown(Shutdown::Both);
                    info!("[voice] utterance sent ({active_ms} ms, reason {reason})");
                    stream = None;
                }
            }
        }
    }
}
