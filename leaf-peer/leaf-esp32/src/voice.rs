//! Voice front-end thread: capture mic audio and evaluate two native wake-word
//! models. A standalone `petito` is resolved locally and plays a random GPIO7
//! tune. `yo petito` streams the following utterance to the paired host for the
//! usual list commands. The RGB LED stays off until a model fires.
//!
//! The loudness (dB) gate only starts local inference; it is not authorization to
//! stream. The short wake is held briefly so the longer phrase can win before
//! local music starts.
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
use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Gpio4, Gpio5, Gpio6, Gpio7};
use esp_idf_svc::hal::i2s::config::{
    Config, DataBitWidth, SlotMode, StdClkConfig, StdConfig, StdGpioConfig, StdSlotConfig,
};
use esp_idf_svc::hal::i2s::{I2sDriver, I2S0};
use esp_idf_svc::hal::ledc::{CHANNEL0 as LEDC_CHANNEL0, TIMER0 as LEDC_TIMER0};
use esp_idf_svc::hal::rmt::CHANNEL0;
use log::{info, warn};

use crate::buzzer::Buzzer;
use crate::led::Led;

const SAMPLE_RATE: u32 = 16_000;
const FULL_SCALE_24BIT: f32 = 8_388_608.0;
const READ_BYTES: usize = 4096; // 1024 frames (32-bit slot) ~= 64 ms @ 16 kHz

// Two native command-input modes. `petito` opens the fast mode; `yo petito`
// opens (or upgrades to) the extended mode. Both are maximums; normal trailing
// silence still ends a completed command earlier.
const FAST_COMMAND_MAX_MS: u32 = 4_000;
const EXTENDED_COMMAND_MAX_MS: u32 = 8_000;
// A `yo petito` utterance can make the shorter `petito` model fire at the end
// of the phrase. Defer the local action long enough for the compound model's
// next streaming windows to cross its cutoff.
const PETITO_DISAMBIGUATION_MS: u32 = 900;
const PREROLL_WINDOWS: usize = 4; // ~256 ms kept before the wake fires
const PREWAKE_WINDOWS: usize = 32; // ~2 s retained locally; never streamed without a native wake

#[inline]
fn command_max_ms(extended_wake_fired: bool) -> u32 {
    if extended_wake_fired {
        EXTENDED_COMMAND_MAX_MS
    } else {
        FAST_COMMAND_MAX_MS
    }
}

// Thresholds come from each model's quantized streaming evaluation and apply to
// the five-inference rolling average in the C shim. They are intentionally not
// shared: class weighting and hard-negative composition calibrate model outputs
// differently even though both use the same uint8/256 output tensor.
const PETITO_PROB_CUTOFF: f32 = 0.78;
const YO_PETITO_PROB_CUTOFF: f32 = 0.30;
const WAKE_CONSECUTIVE_HITS: u8 = 2;

// After an on-device keyword fires, allow a natural pause before the command.
// Until speech resumes after that first pause, use this floor instead of the
// normal 500ms end-of-speech timeout. The mode cap still bounds the exchange.
const POST_WAKE_SILENCE_MS: u32 = 1_500;

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
    let hdr = [
        body_len as u8,
        (body_len >> 8) as u8,
        (body_len >> 16) as u8,
        ftype,
    ];
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
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "feedback deadline",
            ));
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof",
                ))
            }
            Ok(n) => filled += n,
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty frame",
        ));
    }
    let mut body = vec![0u8; body_len];
    read_fill(stream, &mut body, deadline)?;
    Ok((body[0], body[1..].to_vec()))
}

// Drive the LED from host feedback frames after END, until the host says it is
// done (or the socket closes / the feedback window elapses).
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
            Ok(_) => {}      // unknown frame type — ignore
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

// Separate native models let firmware select the correct command duration.
// Both share one microfrontend but retain independent streaming model state.
static PETITO_TFLITE: &[u8] = include_bytes!("../wakeword/petito.tflite");
static YO_PETITO_TFLITE: &[u8] = include_bytes!("../wakeword/yo_petito.tflite");

// Reinterpret a PCM16 window as little-endian wire bytes. The leaf is
// little-endian, so the in-memory layout already IS PCM16LE — no copy/swap.
// Holding the samples as `i16` (not raw bytes) also gives mww_process a properly
// aligned `*const i16` for the Xtensa front-end.
#[inline]
fn pcm16_as_bytes(p: &[i16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, std::mem::size_of_val(p)) }
}

// Feed one captured PCM window to the on-device wake models. The C shim runs the
// vendored microfrontend (pymicro-features-matched) + the streaming int8 model,
// carrying state across calls. We log the per-window probability and the running
// raw feature peak for calibration, and
// on the first window to cross the cutoff we light the LED yellow locally — an
// instant on-device wake confirmation, independent of the host re-confirm that
// drive_feedback() layers on later. Returns the window's max probability (or <0
// when the front-end isn't ready / no full frame landed this window).
fn mww_step(
    pcm16: &[i16],
    petito_music_fired: &mut bool,
    wake_fired: &mut bool,
    extended_wake_fired: &mut bool,
    petito_hits: &mut u8,
    yo_petito_hits: &mut u8,
    led: &mut Led<'_>,
) -> f32 {
    let processed = unsafe {
        esp_idf_svc::sys::mww::mww_process(pcm16.as_ptr(), pcm16.len() as core::ffi::c_int)
    };
    if processed < 0.0 {
        return processed; // front-end stubbed/not ready, or <3 frames this window
    }
    let petito_prob = unsafe { esp_idf_svc::sys::mww::mww_current_prob_slot(0) };
    let yo_petito_prob = unsafe { esp_idf_svc::sys::mww::mww_current_prob_slot(1) };
    let feat_peak = unsafe { esp_idf_svc::sys::mww::mww_last_feat_peak() };
    info!(
        "[voice] mww petito={petito_prob:.3} yo_petito={yo_petito_prob:.3} feat_peak={feat_peak}"
    );
    *petito_hits = if petito_prob >= PETITO_PROB_CUTOFF {
        (*petito_hits).saturating_add(1)
    } else {
        0
    };
    *yo_petito_hits = if yo_petito_prob >= YO_PETITO_PROB_CUTOFF {
        (*yo_petito_hits).saturating_add(1)
    } else {
        0
    };
    if *yo_petito_hits >= WAKE_CONSECUTIVE_HITS && !*extended_wake_fired {
        *extended_wake_fired = true;
        *wake_fired = true;
        let _ = led.yellow();
        info!("[voice] on-device yo-petito wake FIRED ({WAKE_CONSECUTIVE_HITS} consecutive hits, prob={yo_petito_prob:.3} >= {YO_PETITO_PROB_CUTOFF:.2}, feat_peak={feat_peak})");
    }
    if *petito_hits >= WAKE_CONSECUTIVE_HITS && !*petito_music_fired {
        *petito_music_fired = true;
        let _ = led.yellow();
        info!("[voice] on-device petito wake FIRED — pending local music ({WAKE_CONSECUTIVE_HITS} consecutive hits, prob={petito_prob:.3} >= {PETITO_PROB_CUTOFF:.2}, feat_peak={feat_peak})");
    }
    petito_prob.max(yo_petito_prob)
}

pub fn run(
    i2s0: I2S0,
    bclk: Gpio4,
    din: Gpio6,
    ws: Gpio5,
    rmt: CHANNEL0,
    led_pin: AnyOutputPin,
    buzzer_timer: LEDC_TIMER0,
    buzzer_channel: LEDC_CHANNEL0,
    buzzer_pin: Gpio7,
    audio_addr: &'static str,
    wake_db_threshold: f32,
    silence_timeout_ms: u32,
    mic_gain_shift: u32,
) {
    let mut led = match Led::new(rmt, led_pin) {
        Ok(l) => l,
        Err(err) => {
            warn!("[voice] LED init failed ({err:#}); continuing without it");
            return;
        }
    };
    let _ = led.off();
    let mut buzzer = match Buzzer::new(buzzer_timer, buzzer_channel, buzzer_pin) {
        Ok(b) => Some(b),
        Err(err) => {
            warn!("[voice] buzzer init failed ({err:#}); petito music disabled");
            None
        }
    };

    let cfg = StdConfig::new(
        Config::default(),
        StdClkConfig::from_sample_rate_hz(SAMPLE_RATE),
        StdSlotConfig::philips_slot_default(DataBitWidth::Bits32, SlotMode::Mono),
        StdGpioConfig::default(),
    );
    let mut mic = match I2sDriver::new_std_rx(i2s0, &cfg, bclk, din, None::<AnyIOPin>, ws) {
        Ok(m) => m,
        Err(err) => {
            warn!("[voice] I2S init failed: {err:#}");
            return;
        }
    };
    if let Err(err) = mic.rx_enable() {
        warn!("[voice] I2S rx_enable failed: {err:#}");
        return;
    }
    info!("[voice] up: gate>={wake_db_threshold:.0} dBFS, silence {silence_timeout_ms} ms, petito=25-song shuffle, yo-petito command={} ms, mic gain {}x, host {audio_addr}", EXTENDED_COMMAND_MAX_MS, 1u32 << mic_gain_shift);

    // Load the on-device wake model once. rc=0 ok; <0 = model/version/arena/op
    // error (validates AllocateTensors + the 64KB arena + streaming resource vars
    // on real hardware — the #7242 risk). is_wake() inference wires in next.
    let petito_rc = unsafe {
        esp_idf_svc::sys::mww::mww_init_slot(
            0,
            PETITO_TFLITE.as_ptr(),
            PETITO_TFLITE.len() as core::ffi::c_int,
        )
    };
    let yo_petito_rc = unsafe {
        esp_idf_svc::sys::mww::mww_init_slot(
            1,
            YO_PETITO_TFLITE.as_ptr(),
            YO_PETITO_TFLITE.len() as core::ffi::c_int,
        )
    };
    info!(
        "[voice] mww_init petito={petito_rc} ({} bytes), yo_petito={yo_petito_rc} ({} bytes)",
        PETITO_TFLITE.len(),
        YO_PETITO_TFLITE.len()
    );

    let mut buf = vec![0u8; READ_BYTES]; // heap, not stack — this thread runs on a small internal-RAM stack
                                         // Keep parallel pre-rolls: the wake models receive clean, unclipped PCM,
                                         // while the host stream retains the configured digital gain needed by STT.
    let mut wake_preroll: VecDeque<Vec<i16>> = VecDeque::new();
    let mut preroll: VecDeque<Vec<i16>> = VecDeque::new();
    let mut listening = false;
    let mut petito_music_fired = false; // standalone `petito` candidate
    let mut petito_pending_ms = 0u32; // wait for possible `yo petito` upgrade
    let mut wake_fired = false; // specifically the command wake `yo petito`
    let mut extended_wake_fired = false; // specifically `yo petito`
    let mut petito_hits = 0u8;
    let mut yo_petito_hits = 0u8;
    let mut wake_pause_seen = false; // a quiet window occurred after the wake fired
    let mut spoke_after_wake = false; // command speech resumed after that pause
    let mut win_n = 0u32; // audio windows fed to mww this utterance (calibration summary)
    let mut stream: Option<TcpStream> = None;
    let mut wake_stream_started = false;
    let mut prewake_audio: VecDeque<Vec<i16>> = VecDeque::new();
    let mut silent_ms = 0u32;
    let mut listen_ms = 0u32;
    // PCM16 extraction shift: 16 = plain top-16-bits, minus the digital gain.
    let pcm_shift = 16u32.saturating_sub(mic_gain_shift.min(8));
    let _ = led.off();

    loop {
        let n = mic.read(&mut buf[..], BLOCK).unwrap_or(0);
        let frames = n / 4;
        if frames == 0 {
            continue;
        }
        let window_ms = (frames as u32 * 1000) / SAMPLE_RATE;

        // One pass creates two PCM paths from the same 24-bit microphone word:
        // clean PCM16 for microWakeWord (matching the real-device training
        // corpus), and optionally amplified PCM16 for host STT. The old shared
        // +18 dB path clipped 7-71% of calibration samples before wake inference.
        let mut peak: i32 = 0;
        let mut wake_pcm16: Vec<i16> = Vec::with_capacity(frames);
        let mut pcm16: Vec<i16> = Vec::with_capacity(frames);
        for f in 0..frames {
            let o = f * 4;
            let raw = i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
            let mag = (raw >> 8).abs();
            if mag > peak {
                peak = mag;
            }
            wake_pcm16.push((raw >> 16).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            let amplified = (raw >> pcm_shift).clamp(i16::MIN as i32, i16::MAX as i32);
            pcm16.push(amplified as i16);
        }
        let dbfs = if peak > 0 {
            20.0 * (peak as f32 / FULL_SCALE_24BIT).log10()
        } else {
            -120.0
        };

        if !listening {
            // IDLE — LED off, waiting for speech. Keep a short pre-roll.
            wake_preroll.push_back(wake_pcm16);
            preroll.push_back(pcm16);
            while wake_preroll.len() > PREROLL_WINDOWS {
                wake_preroll.pop_front();
            }
            while preroll.len() > PREROLL_WINDOWS {
                preroll.pop_front();
            }
            if dbfs >= wake_db_threshold {
                // Sound only opens a local inference window. No socket is opened
                // and no audio leaves the Leaf until a native model fires.
                info!("[voice] sound gate open ({dbfs:.1} dBFS) — evaluating locally");
                listening = true;
                petito_music_fired = false;
                petito_pending_ms = 0;
                wake_fired = false;
                extended_wake_fired = false;
                petito_hits = 0;
                yo_petito_hits = 0;
                wake_pause_seen = false;
                spoke_after_wake = false;
                win_n = 0;
                listen_ms = 0;
                silent_ms = 0;
                wake_stream_started = false;
                prewake_audio.clear();
                // Start a fresh wake-detection window and warm the streaming model
                // with the pre-roll lead-in before the captured audio arrives.
                unsafe { esp_idf_svc::sys::mww::mww_reset() };
                for p in wake_preroll.iter() {
                    mww_step(
                        p,
                        &mut petito_music_fired,
                        &mut wake_fired,
                        &mut extended_wake_fired,
                        &mut petito_hits,
                        &mut yo_petito_hits,
                        &mut led,
                    );
                    win_n += 1;
                }
                for p in preroll.iter() {
                    prewake_audio.push_back(p.clone());
                }
                wake_preroll.clear();
                preroll.clear();
            }
        } else {
            // CAPTURING — run both on-device wake models over each window. Before
            // a native match, audio only enters the bounded local pre-wake buffer.
            // After a match, stream it and count down to silence or the mode cap.
            if !wake_fired {
                prewake_audio.push_back(pcm16.clone());
                while prewake_audio.len() > PREWAKE_WINDOWS {
                    prewake_audio.pop_front();
                }
            }
            let was_awake = wake_fired;
            let was_extended = extended_wake_fired;
            mww_step(
                &wake_pcm16,
                &mut petito_music_fired,
                &mut wake_fired,
                &mut extended_wake_fired,
                &mut petito_hits,
                &mut yo_petito_hits,
                &mut led,
            );
            if petito_music_fired && !wake_fired {
                petito_pending_ms = petito_pending_ms.saturating_add(window_ms);
            } else if wake_fired {
                petito_pending_ms = 0;
            }
            if extended_wake_fired && !was_extended {
                // Wake just fired: open the post-wake grace so the user's pause
                // after the longer wake phrase doesn't close the window, and extend
                // this capture from the 4s fast mode to the 8s command mode.
                silent_ms = 0;
                listen_ms = 0;
                info!("[voice] capture mode extended to {EXTENDED_COMMAND_MAX_MS} ms (yo fired)");
            }
            win_n += 1;

            // The model can fire while warming on the idle pre-roll, so this is
            // tracked independently from `was_awake` (which only describes the
            // current captured window).
            if wake_fired && !wake_stream_started {
                wake_stream_started = true;
                // Command limits start after the wake phrase, not when the cheap
                // sound gate first opened during that phrase.
                listen_ms = 0;
                silent_ms = 0;
                stream = connect_any(audio_addr);
                match stream {
                    Some(ref mut s) => {
                        s.set_nodelay(true).ok();
                        let _ = send_frame(s, F_HELLO, b"leaf");
                        let wake_id = if extended_wake_fired { 2 } else { 1 };
                        let _ = send_frame(s, F_START, &[wake_id, 0, 0, 0, 0]);
                        for p in prewake_audio.drain(..) {
                            let _ = send_frame(s, F_CHUNK, pcm16_as_bytes(&p));
                        }
                        info!("[voice] native wake accepted; streaming to host (wakeWordId={wake_id})");
                    }
                    None => {
                        warn!(
                            "[voice] native wake fired but no audio host reachable in {audio_addr}"
                        );
                        prewake_audio.clear();
                    }
                }
            }
            if let Some(ref mut s) = stream {
                // The firing window is already present in prewake_audio and was
                // drained above. Subsequent command windows are sent directly.
                if was_awake && send_frame(s, F_CHUNK, pcm16_as_bytes(&pcm16)).is_err() {
                    warn!("[voice] stream dropped");
                    stream = None;
                }
            }

            listen_ms += window_ms;
            // Silence = below the wake level: the command stays alive while sound
            // exceeds the gate and ends once it drops back to ambient. (A fixed
            // lower floor never registered silence in a normal-noise room.)
            if dbfs < wake_db_threshold {
                silent_ms += window_ms;
                if wake_fired {
                    wake_pause_seen = true;
                }
            } else {
                silent_ms = 0;
                if wake_fired && wake_pause_seen {
                    spoke_after_wake = true;
                }
            }

            // A standalone `petito` is local-only. Pause I2S while the piezo is
            // active so its own notes cannot enter the wake pipeline, then reset
            // the streaming models and return to idle. `yo petito` always wins
            // if it fires during the disambiguation window above.
            if petito_music_fired && !wake_fired && petito_pending_ms >= PETITO_DISAMBIGUATION_MS {
                info!("[voice] standalone petito confirmed — choosing the next shuffled tune");
                let mic_paused = match mic.rx_disable() {
                    Ok(()) => true,
                    Err(err) => {
                        warn!("[voice] could not pause mic for music: {err:#}");
                        false
                    }
                };
                let _ = led.green();
                match buzzer.as_mut() {
                    Some(player) => match player.play_random() {
                        Ok(tune) => info!("[voice] random tune complete: {}", tune.title),
                        Err(err) => {
                            warn!("[voice] random tune failed: {err:#}");
                            let _ = led.red();
                        }
                    },
                    None => {
                        warn!("[voice] petito music requested but buzzer is unavailable");
                        let _ = led.red();
                        std::thread::sleep(Duration::from_millis(600));
                    }
                }
                if mic_paused {
                    std::thread::sleep(Duration::from_millis(150));
                    if let Err(err) = mic.rx_enable() {
                        warn!("[voice] could not resume mic after music: {err:#}");
                        return;
                    }
                }
                unsafe { esp_idf_svc::sys::mww::mww_reset() };
                let _ = led.off();
                stream = None;
                wake_preroll.clear();
                preroll.clear();
                prewake_audio.clear();
                listening = false;
                petito_music_fired = false;
                petito_pending_ms = 0;
                wake_fired = false;
                extended_wake_fired = false;
                petito_hits = 0;
                yo_petito_hits = 0;
                continue;
            }

            // Effective end-of-utterance silence budget: after a native wake but
            // before command speech resumes, leave enough time to notice the
            // yellow light and continue. Afterwards use normal end-of-speech.
            let silence_budget = if petito_music_fired && !wake_fired {
                (PETITO_DISAMBIGUATION_MS + 250).max(silence_timeout_ms)
            } else if wake_fired && !spoke_after_wake {
                POST_WAKE_SILENCE_MS.max(silence_timeout_ms)
            } else {
                silence_timeout_ms
            };

            let max_command_ms = command_max_ms(extended_wake_fired);
            if silent_ms >= silence_budget || listen_ms >= max_command_ms {
                // One conclusive calibration line per utterance. invokes==0 ⇒ the
                // frontend produced no full 3-frame group (stub firmware / starved):
                // reflash or chase the front-end. invokes≥1 with max_prob in 0..0.9
                // ⇒ the model ran but didn't fire: a feature-scale / cutoff tune,
                // NOT a front-end problem. max_prob≥cutoff ⇒ yellow fired.
                let petito_inv = unsafe { esp_idf_svc::sys::mww::mww_last_invokes_slot(0) };
                let yo_petito_inv = unsafe { esp_idf_svc::sys::mww::mww_last_invokes_slot(1) };
                let petito_prob = unsafe { esp_idf_svc::sys::mww::mww_last_prob_slot(0) };
                let yo_petito_prob = unsafe { esp_idf_svc::sys::mww::mww_last_prob_slot(1) };
                let max_prob = petito_prob.max(yo_petito_prob);
                let max_feat = unsafe { esp_idf_svc::sys::mww::mww_last_feat_peak() };
                info!("[voice] WAKE SUMMARY windows={win_n} petito(inv={petito_inv},p={petito_prob:.3},cutoff={PETITO_PROB_CUTOFF:.2},music={petito_music_fired}) yo_petito(inv={yo_petito_inv},p={yo_petito_prob:.3},cutoff={YO_PETITO_PROB_CUTOFF:.2}) max_feat_peak={max_feat} fired={wake_fired} extended={extended_wake_fired}");
                let reason = if listen_ms >= max_command_ms {
                    1u8
                } else {
                    0u8
                };
                if let Some(ref mut s) = stream {
                    // END carries the on-device wake label so the host can store
                    // each utterance into the training dataset auto-labeled as a
                    // native match: [reason, fired, prob_milli:u16le, feat_peak:u16le].
                    let prob_milli = (max_prob.max(0.0) * 1000.0) as u16;
                    let end_payload = [
                        reason,
                        wake_fired as u8,
                        prob_milli as u8,
                        (prob_milli >> 8) as u8,
                        max_feat as u8,
                        (max_feat >> 8) as u8,
                    ];
                    let _ = send_frame(s, F_END, &end_payload);
                    let _ = s.flush();
                    let mode = if extended_wake_fired {
                        "yo-petito/extended"
                    } else {
                        "petito/fast"
                    };
                    info!("[voice] utterance sent ({listen_ms} ms, mode {mode}, max {max_command_ms} ms, reason {reason}); awaiting host feedback");
                    // Mirror the host's recognition onto the LED (yellow wake →
                    // purple command → green saved). Then a brief hold so the
                    // final color registers before we reset to idle.
                    drive_feedback(s, &mut led);
                    std::thread::sleep(Duration::from_millis(500));
                    let _ = s.shutdown(Shutdown::Both);
                } else {
                    info!("[voice] capture window ended ({listen_ms} ms, max {max_command_ms} ms, no host)");
                }
                let _ = led.off();
                stream = None;
                prewake_audio.clear();
                listening = false;
            }
        }
    }
}
