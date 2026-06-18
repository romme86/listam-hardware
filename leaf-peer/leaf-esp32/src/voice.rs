//! Voice front-end thread: capture mic audio, gate on loudness, and stream an
//! utterance to the paired host for transcription. The RGB LED is NOT driven by
//! the local loudness gate — it stays off while capturing and lights only when
//! the host reports what it recognized (yellow wake word → purple command →
//! green saved), streamed back over the same socket after END.
//!
//! The loudness (dB) gate is a capture trigger, not a wake-word match: a loud
//! enough sound opens an utterance and streams it, but the board gives no light
//! feedback until the host actually recognizes the wake word. The custom
//! microWakeWord models ("yo" / "hey listam" / "dai dai dai dai") plug in where
//! `is_wake()` is computed (Task 8); the dB gate stays in front regardless.
//!
//! Wire frames to the host audio bridge: [u24le bodyLen][type][payload]
//!   leaf -> host: 0x00 hello(utf8 id) · 0x01 start(wakeWordId:u8, epochMs:u32le)
//!                 0x02 chunk(PCM16LE 16k mono)
//!                 0x03 end(reason:u8 0=silence/1=max, fired:u8, probMilli:u16le, featPeak:u16le)
//!   host -> leaf: 0x10 led(color:u8 0=off/1=yellow/2=purple/3=green/4=red)
//!                 0x11 done (host finished; reset to idle)

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

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

// On-device microWakeWord fire threshold. The model emits uint8/256 probability;
// 0.90 ≈ raw uint8 ≥ 230. Tune alongside the /64 feature scale once calibrated.
const WAKE_PROB_CUTOFF: f32 = 0.90;

const F_HELLO: u8 = 0x00;
const F_START: u8 = 0x01;
const F_CHUNK: u8 = 0x02;
const F_END: u8 = 0x03;

// Host -> leaf response frames (same [u24le bodyLen][type][payload] framing).
const R_LED: u8 = 0x10; // payload[0] = color: 0 off, 1 yellow, 2 purple, 3 green, 4 red
const R_DONE: u8 = 0x11; // host finished processing this utterance

// How long to wait for the host's recognition feedback after END before giving
// up and resetting to idle (whisper transcription dominates this — be generous).
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(25);
// Per-read socket timeout: short, so a quiet stretch returns WouldBlock quickly
// and we re-check the wall-clock deadline rather than blocking the whole budget.
const FEEDBACK_POLL: Duration = Duration::from_millis(500);

fn send_frame(stream: &mut TcpStream, ftype: u8, payload: &[u8]) -> std::io::Result<()> {
    let body_len = 1 + payload.len();
    let hdr = [body_len as u8, (body_len >> 8) as u8, (body_len >> 16) as u8, ftype];
    stream.write_all(&hdr)?;
    stream.write_all(payload)
}

// Fill `buf` fully, waiting across short per-read timeouts until `deadline`.
// WouldBlock/TimedOut/Interrupted mean "no data yet, keep waiting" (NOT EOF) —
// so a slow host or a frame split across TCP segments doesn't abort feedback.
// Only a real connection close (read == 0) / error or the deadline ends it.
fn read_fill(stream: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "feedback deadline"));
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof")),
            Ok(n) => filled += n,
            Err(ref e) if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
            ) => {} // no data this poll — re-check the deadline and retry
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// Read one host -> leaf frame: [u24le bodyLen][type][payload], waiting up to
// `deadline` across short read polls. Returns (type, payload).
fn read_frame(stream: &mut TcpStream, deadline: Instant) -> std::io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 3];
    read_fill(stream, &mut hdr, deadline)?;
    let body_len = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
    if body_len == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "empty frame"));
    }
    let mut body = vec![0u8; body_len];
    read_fill(stream, &mut body, deadline)?;
    Ok((body[0], body[1..].to_vec()))
}

// Drive the LED from host feedback frames after END, until the host says it is
// done (or the socket closes / the feedback window elapses). The LED is off on
// entry and is left off by the caller on return.
fn drive_feedback(stream: &mut TcpStream, led: &mut Led<'_>) {
    stream.set_read_timeout(Some(FEEDBACK_POLL)).ok();
    let deadline = Instant::now() + FEEDBACK_TIMEOUT;
    loop {
        match read_frame(stream, deadline) {
            Ok((R_LED, payload)) => {
                let color = payload.first().copied().unwrap_or(0);
                let _ = match color {
                    1 => led.yellow(),
                    2 => led.purple(),
                    3 => led.green(),
                    4 => led.red(),
                    _ => led.off(),
                };
            }
            Ok((R_DONE, _)) => break,
            Ok(_) => {} // unknown frame type — ignore
            Err(_) => break, // real EOF / error / deadline reached
        }
    }
}

// Try each comma-separated host:port (like hub_addr) with a bounded timeout, so
// an unreachable address can't hang the voice thread.
fn connect_any(list: &str) -> Option<TcpStream> {
    for addr in list.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Ok(mut sas) = addr.to_socket_addrs() {
            if let Some(sa) = sas.next() {
                if let Ok(s) = TcpStream::connect_timeout(&sa, Duration::from_millis(600)) {
                    return Some(s);
                }
            }
        }
    }
    None
}

// On-device microWakeWord "yo" model (int8 streaming MixedNet, 61KB). Run as a
// first-stage filter behind the dB gate via the mww C-shim (components/mww).
static YO_TFLITE: &[u8] = include_bytes!("../wakeword/yo.tflite");

// Reinterpret a PCM16 window as little-endian wire bytes. The leaf is
// little-endian, so the in-memory layout already IS PCM16LE — no copy/swap.
// Holding the samples as `i16` (not raw bytes) also gives mww_process a properly
// aligned `*const i16` for the Xtensa front-end.
#[inline]
fn pcm16_as_bytes(p: &[i16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, std::mem::size_of_val(p)) }
}

// Feed one captured PCM window to the on-device wake model. The C shim runs the
// vendored microfrontend (pymicro-features-matched) + the streaming int8 model,
// carrying state across calls. We log the per-window probability and the running
// raw feature peak so a single spoken "yo" calibrates the /64 feature scale, and
// on the first window to cross the cutoff we light the LED yellow locally — an
// instant on-device wake confirmation, independent of the host re-confirm that
// drive_feedback() layers on later. Returns the window's max probability (or <0
// when the front-end isn't ready / no full frame landed this window).
fn mww_step(pcm16: &[i16], wake_fired: &mut bool, led: &mut Led<'_>) -> f32 {
    let prob = unsafe {
        esp_idf_svc::sys::mww::mww_process(pcm16.as_ptr(), pcm16.len() as core::ffi::c_int)
    };
    if prob < 0.0 {
        return prob; // front-end stubbed/not ready, or <3 frames this window
    }
    let feat_peak = unsafe { esp_idf_svc::sys::mww::mww_last_feat_peak() };
    info!("[voice] mww prob={prob:.3} feat_peak={feat_peak}");
    if prob >= WAKE_PROB_CUTOFF && !*wake_fired {
        *wake_fired = true;
        let _ = led.yellow();
        info!("[voice] on-device wake FIRED (prob={prob:.3} >= {WAKE_PROB_CUTOFF:.2}, feat_peak={feat_peak})");
    }
    prob
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
    info!("[voice] up: gate>={wake_db_threshold:.0} dBFS, silence {silence_timeout_ms} ms, host {audio_addr}");

    // Load the on-device wake model once. rc=0 ok; <0 = model/version/arena/op
    // error (validates AllocateTensors + the 64KB arena + streaming resource vars
    // on real hardware — the #7242 risk). is_wake() inference wires in next.
    let mww_rc = unsafe {
        esp_idf_svc::sys::mww::mww_init(YO_TFLITE.as_ptr(), YO_TFLITE.len() as core::ffi::c_int)
    };
    info!("[voice] mww_init rc={mww_rc} ({} byte model)", YO_TFLITE.len());

    let mut buf = vec![0u8; READ_BYTES]; // heap, not stack — this thread runs on a small internal-RAM stack
    let mut preroll: VecDeque<Vec<i16>> = VecDeque::new();
    let mut listening = false;
    let mut wake_fired = false; // on-device mww crossed the cutoff this utterance
    let mut win_n = 0u32; // audio windows fed to mww this utterance (calibration summary)
    let mut stream: Option<TcpStream> = None;
    let mut silent_ms = 0u32;
    let mut listen_ms = 0u32;
    let _ = led.off();

    loop {
        let n = mic.read(&mut buf[..], BLOCK).unwrap_or(0);
        let frames = n / 4;
        if frames == 0 { continue; }
        let window_ms = (frames as u32 * 1000) / SAMPLE_RATE;

        // One pass: peak (24-bit, for dBFS) + PCM16 (top 16 bits of each slot word).
        // Keep samples as i16 — wire bytes come from pcm16_as_bytes() and the wake
        // model wants an aligned *const i16.
        let mut peak: i32 = 0;
        let mut pcm16: Vec<i16> = Vec::with_capacity(frames);
        for f in 0..frames {
            let o = f * 4;
            let raw = i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
            let mag = (raw >> 8).abs();
            if mag > peak { peak = mag; }
            pcm16.push((raw >> 16) as i16);
        }
        let dbfs = if peak > 0 { 20.0 * (peak as f32 / FULL_SCALE_24BIT).log10() } else { -120.0 };

        if !listening {
            // IDLE — LED off, waiting for the wake word. Keep a short pre-roll.
            preroll.push_back(pcm16);
            while preroll.len() > PREROLL_WINDOWS { preroll.pop_front(); }
            if dbfs >= wake_db_threshold {
                // Sound opened the capture window. The dB gate is only a cheap
                // pre-filter; the on-device microWakeWord model decides whether
                // this is really "yo" and lights the LED yellow locally when it
                // fires. The host re-confirms and drives the later colors.
                info!("[voice] sound gate open ({dbfs:.1} dBFS) — capturing");
                listening = true;
                wake_fired = false;
                win_n = 0;
                listen_ms = 0;
                silent_ms = 0;
                // Start a fresh wake-detection window and warm the streaming model
                // with the pre-roll lead-in before the captured audio arrives.
                unsafe { esp_idf_svc::sys::mww::mww_reset() };
                for p in preroll.iter() { mww_step(p, &mut wake_fired, &mut led); win_n += 1; }
                // Connect if a host is reachable; either way we capture locally for
                // the command window, so the countdown works with no host too.
                stream = connect_any(audio_addr);
                match stream {
                    Some(ref mut s) => {
                        s.set_nodelay(true).ok();
                        let _ = send_frame(s, F_HELLO, b"leaf");
                        let _ = send_frame(s, F_START, &[1, 0, 0, 0, 0]); // wakeWordId=1 (dB gate)
                        for p in preroll.drain(..) { let _ = send_frame(s, F_CHUNK, pcm16_as_bytes(&p)); }
                        info!("[voice] streaming to host");
                    }
                    None => {
                        warn!("[voice] no audio host reachable in {audio_addr}");
                        preroll.clear();
                    }
                }
            }
        } else {
            // CAPTURING — run the on-device wake model over each window (lights the
            // LED yellow locally the instant "yo" is matched) and stream the audio,
            // counting down to the end of the command: silence_timeout_ms of quiet,
            // or the max window.
            mww_step(&pcm16, &mut wake_fired, &mut led);
            win_n += 1;
            if let Some(ref mut s) = stream {
                if send_frame(s, F_CHUNK, pcm16_as_bytes(&pcm16)).is_err() {
                    warn!("[voice] stream dropped");
                    stream = None;
                }
            }

            listen_ms += window_ms;
            // Silence = below the wake level: the command stays alive while sound
            // exceeds the gate and ends once it drops back to ambient. (A fixed
            // lower floor never registered silence in a normal-noise room.)
            if dbfs < wake_db_threshold { silent_ms += window_ms; } else { silent_ms = 0; }

            if silent_ms >= silence_timeout_ms || listen_ms >= MAX_UTTERANCE_MS {
                // One conclusive calibration line per utterance. invokes==0 ⇒ the
                // frontend produced no full 3-frame group (stub firmware / starved):
                // reflash or chase the front-end. invokes≥1 with max_prob in 0..0.9
                // ⇒ the model ran but didn't fire: a feature-scale / cutoff tune,
                // NOT a front-end problem. max_prob≥cutoff ⇒ yellow fired.
                let inv = unsafe { esp_idf_svc::sys::mww::mww_last_invokes() };
                let max_prob = unsafe { esp_idf_svc::sys::mww::mww_last_prob() };
                let max_feat = unsafe { esp_idf_svc::sys::mww::mww_last_feat_peak() };
                info!("[voice] WAKE SUMMARY windows={win_n} invokes={inv} max_prob={max_prob:.3} max_feat_peak={max_feat} fired={wake_fired} (cutoff={WAKE_PROB_CUTOFF:.2})");
                let reason = if listen_ms >= MAX_UTTERANCE_MS { 1u8 } else { 0u8 };
                if let Some(ref mut s) = stream {
                    // END carries the on-device wake label so the host can store
                    // each utterance into the training dataset auto-labeled as a
                    // positive ("yo" fired) or a hard-negative (gate opened, model
                    // didn't fire): [reason, fired, prob_milli:u16le, feat_peak:u16le].
                    let prob_milli = (max_prob.max(0.0) * 1000.0) as u16;
                    let end_payload = [
                        reason,
                        wake_fired as u8,
                        prob_milli as u8, (prob_milli >> 8) as u8,
                        max_feat as u8, (max_feat >> 8) as u8,
                    ];
                    let _ = send_frame(s, F_END, &end_payload);
                    let _ = s.flush();
                    info!("[voice] utterance sent ({listen_ms} ms, reason {reason}); awaiting host feedback");
                    // Mirror the host's recognition onto the LED (yellow wake →
                    // purple command → green saved). Then a brief hold so the
                    // final color registers before we reset to idle.
                    drive_feedback(s, &mut led);
                    std::thread::sleep(Duration::from_millis(500));
                    let _ = s.shutdown(Shutdown::Both);
                } else {
                    info!("[voice] capture window ended ({listen_ms} ms, no host)");
                }
                let _ = led.off();
                stream = None;
                listening = false;
            }
        }
    }
}
