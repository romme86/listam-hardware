# On-device wake-word plan (microWakeWord) for the ESP32-S3 leaf

Goal: replace the crude loudness gate (`voice.rs:204 if dbfs >= wake_db_threshold`)
with a real keyword-spotting model so the leaf only streams to the host when
actually addressed — killing the ambient false-wakes we kept fighting with the
`-40`/`-30`/`-37` threshold tuning.

## TL;DR decisions
- **Do NOT ship bare "yo".** It's ~2 phonemes, open-vowel, below the ~6-phoneme
  floor, and overlaps tons of speech (you/yeah/hello/no/oh/go/so/whoa) → heavy
  false-accepts. **Recommend "hey listam"** (~7-8 phonemes, distinct trajectory,
  lands in the <0.2 false-accepts/hour regime of shipped models). Fallback if a
  short trigger is mandatory: **"dai dai dai dai"** (reduplication helps, but
  vowel-heavy + timing-sensitive).
- **You do NOT record "yo" hundreds of times.** Positives are 100% **synthetic
  TTS** (Piper); negatives are **free pre-existing datasets**. Recording your own
  voice is optional (~20-50 clips) as a late accuracy nudge.
- **Scope caveat:** this fixes **false-WAKES only**. After wake fires you still
  stream the same distant/noisy audio to whisper, so far-field *command*
  transcription SNR is unchanged. That needs a mic array / closer use / denoise.

## Data sourcing (the "record vs free" question)
- **Positives:** `rhasspy/piper-sample-generator` (dscripka fork). Smoke test
  `--max-samples 1`, then generate **30k-50k+** for a short word (notebook default
  1000 is too few). 904 LibriTTS speakers (`--max-speakers` high but <904); vary
  `--length-scales` (speed) and noise/blend for prosody. Train-time augmentation
  (from mWW `basic_training_notebook`): AddBackgroundNoise p=0.75 (SNR -5..10),
  RIR p=0.5, Gain p=1.0, PitchShift/Distortion/EQ each p=0.1, + SpecAugment;
  clip_duration_ms=1500. Frontend MUST be `pymicro-features` (matches on-device).
- **Negatives (all free):** fastest = pre-computed feature bundle
  `hf.co/datasets/kahrendt/microwakeword` (~9.7 GB, **CC-BY-NC** → personal OK,
  not commercial). Families: music (FMA-medium), sound events (FSD50K), noise
  (WHAM!), not-the-wake-word speech (LibriSpeech, VOiCES far-field), RIRs (MIT
  environmental, 271 IRs). **Commercial-clean** rebuild: Common Voice (CC0) +
  LibriSpeech/VOiCES (CC-BY) + CC-BY FMA + CC0/CC-BY FSD50K + BIRD RIRs (MIT).
- **Hard negatives for "yo":** add clips of near-homophones (you/yeah/your/hello/
  no/oh/go/so/whoa; die/day if "dai") into the speech-background set.
- **Your own voice:** optional. ~20-50 personal clips into positives "significantly
  improves detection"; or an openWakeWord-style custom verifier (min 3 positive +
  ~10 s non-wake speech + ~5 s kitchen background, trains <5 min). Train from
  synthetic+free first, measure, only record if numbers require it.

## Stages
### Stage 0 — Decide phrase + training box (~half a day; GPU mandatory)
- Decide phrase (hey listam recommended). Pick GPU env: free Colab T4 (documented
  path) or `TaterTotterson/microWakeWord-Trainer-AppleSilicon` (Piper is Linux-y).
  No CPU-only.
- `git clone OHF-Voice/micro-wake-word` (old kahrendt/microWakeWord URL resolves) +
  `rhasspy/piper-sample-generator`. Deps: py>=3.10, tensorflow>=2.16, audiomentations,
  datasets, mmap_ninja, pymicro-features, webrtcvad-wheels, ai-edge-litert.

### Stage 1 — Generate positives + assemble negatives
- Generate 30k-50k+ synthetic positives. Add hard negatives. Download HF feature
  bundle (or rebuild commercial-clean). Apply augmentation per notebook.

### Stage 2 — Feature extraction + train (10k steps, GPU-bound, iterate)
- 40-feature spectrograms (`SpectrogramGeneration`, step_ms=10, slide_frames=10
  train / 1 streaming-test). Train non-streaming **MixedNet (MixConv)** backbone:
  first_conv 32/k5/stride3, pointwise 64×4, mixconv kernels [5],[7,11],[9,15],[23];
  training_steps=[10000], batch=128, lr=0.001, **negative_class_weight=[20]** (keep
  high / raise for short word). Convert to streaming, pick best weights
  (min false-accepts first, then false-rejects).

### Stage 3 — Quantize, export, tune
- int8 export → `stream_state_internal_quant.tflite` (tens-to-hundreds KB). Tune
  `probability_cutoff` HIGH (shipped 0.97, higher for short word) and
  `sliding_window_size`. Build ESPHome JSON manifest (type=micro, model, micro.
  {probability_cutoff, sliding_window_size, feature_step_size:10, tensor_arena_size}).

### Stage 4 — Validate with the right metric
- Measure **false-accepts/HOUR** on conversational background (not just hit-rate).
  Anchor: shipped models ≤0.16 FA/hr on DiPCo; a 2-phoneme "yo" won't approach this
  without recall-costing tuning. Then check false-reject at the chosen FA/hr.

### Stage 5 — On-device integration (leaf firmware, ~1-2 focused days)
- Add `espressif/esp-tflite-micro` (v1.*, esp-nn bundled) via a net-new
  `[[package.metadata.esp-idf-sys.extra_components]]` block in `leaf-esp32/Cargo.toml`.
  **PIN esp-nn** (newer versions broke arena sizing).
- Write a tiny `extern "C"` C++ shim (local component via `component_dirs`):
  `mww_init(model,len,arena,arena_len)` + `float mww_infer(features,n)` wrapping
  MicroInterpreter + op resolver + micro frontend. Don't bind the C++ API directly.
  Ship .tflite via `include_bytes!`.
- Tensor arena: streaming mWW is tiny (~25 KB, okay_nabu=26080). Put it in
  **internal RAM**, over-provision (start 32-48 KB), probe-and-double on failure.
  **#7242 is an over-provision+pin issue, NOT a move-to-PSRAM issue.**
- Frontend: feed existing 16 kHz i16 PCM (`raw>>16`, voice.rs:196) into the micro
  frontend in 160-sample (10 ms) strides, 40 feats/stride; <10 ms/inference on S3.
- voice.rs change at **line 204**: `if dbfs >= wake_db_threshold && is_wake(features) {`
  — keep the dB gate as a cheap pre-filter, AND the model in (runs only on loud
  frames → saves power). Bump the F_START wakeWordId (line 219, currently `[1,..]`
  "dB gate"). Keep the **leaf→host two-stage cascade**: host whisper re-confirms +
  add a refractory/debounce after a fire — the strongest short-word mitigation.

## Risks
- "yo" below phoneme floor → can't hit <0.2 FA/hr without losing recall.
- esp-nn drift breaks arena sizing → pin it. #7242 arena allocator quirk → over-provision.
- Op-resolver must cover every op the model uses or init fails.
- Frontend mismatch (must be pymicro-features, step=10) silently degrades accuracy.
- NC license on convenient HF bundles → rebuild for commercial.
- Net-new managed-component + C++ shim in a pure esp-idf-svc std project; watch the
  known stale `sdkconfig.defaults` partition-path gotcha.

## Open decisions for the user
1. **Phrase**: "hey listam" (rec) / keep "yo" (needs verifier + heavy tuning) / "dai dai dai dai".
2. Personal/research vs commercial (gates negative-dataset licensing).
3. Record own voice? (rec: train synthetic-first, measure, then decide).
4. Training box: Colab T4 vs Apple-Silicon fork.
5. Augment (rec) vs replace the dB gate.
6. Target false-accepts/hour operating point.
7. Add host whisper re-confirm + refractory now (rec) or defer.

---

## Stage 5 progress + the frontend blocker (2026-06-18)

**Done & validated on real ESP32-S3 hardware:**
- `esp-tflite-micro` (managed component) + the C-shim (`components/mww/`) + Rust FFI
  compile & link for Xtensa; `mww_init rc=0` — the int8 streaming model LOADS on
  device (13 ops incl. streaming resource-vars, AllocateTensors, 64 KB **PSRAM**
  arena). Model embedded via `include_bytes!`; init in `voice::run`.
- Build gotchas hit & solved: esp-idf-sys caches the IDF build (`rm -rf
  target/.../build/esp-idf-sys-*` to reprocess `extra_components`); `espflash
  flash <elf>` doesn't write the partition table (use `--partition-table
  partitions.csv`); the 64 KB arena must be PSRAM (`heap_caps_malloc(SPIRAM)`),
  not static .bss (starves internal-RAM task stacks).
- RAM: esp-tflite-micro's footprint fragments internal RAM so the 96 KB hypercore
  **mirror** thread can't get a contiguous stack → made its spawn non-fatal
  (main.rs) so the 16 KB voice thread + model still run. Mirror is DOWN while the
  model is linked; coexistence = a later stack/heap tune.

**Frontend params (must match training = pymicro-features):** 16 kHz, window 30 ms,
step 10 ms, 40 channels, bands 125–7500 Hz, PCAN on, out uint16. Feature→int8:
`int8 = round(raw_uint16 / 64 / 0.10196) − 128` (pymicro returns raw/64; model
input scale 0.10196, zp −128; verified features land in [0, 25.4]). Model consumes
[1,3,40] = 3 frames/inference, streaming state across calls. Cutoff 0.90 → raw
uint8 output ≥ 230.

**BLOCKER (was the last piece):** modern `esp-tflite-micro` **removed the
experimental microfrontend C lib** (`tensorflow/lite/experimental/microfrontend/
lib/frontend.h` is gone) — replaced by an audio-preprocessor **tflite model** +
the new `signal/` DSP lib. So `FrontendProcessSamples` wasn't available and the
shim's frontend was `#if __has_include(frontend.h)`-guarded into a stub
(`mww_process` returned −1; model still loaded).

### Frontend blocker RESOLVED — vendored microfrontend (2026-06-18)

Took **option 1 (vendor the microfrontend C source)** — the exact code
pymicro-features wraps → guaranteed feature match.

- **Vendored from `rhasspy/pymicro-features` (main):**
  - `components/mww/tensorflow/lite/experimental/microfrontend/lib/` — all headers
    + the 16 build `.cc` (fft, fft_util, filterbank(+util), frontend(+util),
    log_lut, log_scale(+util), noise_reduction(+util), pcan_gain_control(+util),
    window(+util), kiss_fft_int16).
  - `components/mww/kissfft/` — pymicro's **fixed-point** kissfft (`kiss_fft.h/.cc`,
    `_kiss_fft_guts.h`, `tools/kiss_fftr.h/.cc`). The wrapper `kiss_fft_int16.cc`
    `#include`s these `.cc` into the `kissfft_fixed16` namespace, so the 2 standalone
    kissfft sources pymicro's setup.py also lists are redundant and omitted.
- **No collision with esp-tflite-micro's own kissfft:** esp uses namespace
  `kiss_fft_fixed16` and only ships `.c`; pymicro uses `kissfft_fixed16` and
  `#include`s `.cc` → distinct symbols, distinct filenames, unambiguous resolution.
- **No global `-DFIXED_POINT`:** the wrappers `#define` it locally; verified the
  rest of the frontend never references it.
- **CMakeLists** (`components/mww/CMakeLists.txt`): explicit SRCS = `mww.cc` + the
  16 frontend `.cc`; `INCLUDE_DIRS "." "kissfft"`; mirrors esp-tflite-micro's
  warning suppressions for the TF sources. The `.` include dir makes
  `frontend.h` resolve → `__has_include` flips `MWW_HAVE_FRONTEND` on → real
  `mww_process()`.
- **Host smoke-compiled** (clang, `-I . -I kissfft`, no global FIXED_POINT):
  `FrontendPopulateState` + `FrontendProcessSamples` run, emit 40 uint16 feats,
  raw peak ~665 (→ `/64` = 10.4, inside the expected `[0, 25.4]`). Init-time
  `malloc` only (in the `*_util.cc`); `FrontendProcessSamples` is per-frame
  alloc-free.
- **esp-idf-sys cache gotcha:** changing a `component_dirs` CMakeLists/sources does
  NOT re-trigger the build script. A plain `cargo build` relinks stale libs in
  ~8 s and silently keeps the stub. Force the C rebuild by deleting
  `target/<triple>/release/.fingerprint/esp-idf-sys-<hash>/run-build-script-*`
  (re-runs the build script → cmake reconfigure → ninja compiles the new sources
  incrementally; `out/build` object cache is preserved, so it's NOT a full esp-idf
  rebuild). `rm -rf …/build/esp-idf-sys-*` also works but is the slow path.

### voice.rs wired (2026-06-18)

On dB-gate open: `mww_reset()`, then feed the pre-roll lead-in + every captured
window to `mww_process()` (`mww_step()` helper). Each window logs
`prob` + `mww_last_feat_peak()` so **one spoken "yo" calibrates the `/64` feature
scale**. On the first window with **prob ≥ 0.90** (`WAKE_PROB_CUTOFF`) it lights
`led.yellow()` locally — the instant on-device wake confirmation ("yellow only on
wake" ask), independent of the host re-confirm that `drive_feedback()` layers on
after END. PCM is kept as `Vec<i16>` (aligned `*const i16` for the front-end; wire
bytes via `pcm16_as_bytes`).

**Deliberately NOT yet done — streaming still gates on the dB pre-filter, not the
model.** Gating the host stream on the mww fire (as the original Stage-5 note
suggested) is deferred until the feature scale is calibrated and the gate is
trusted: until then an off-scale model would silently block the whole voice path
and you'd have nothing to compare the logged probs against. Flip streaming to gate
on `wake_fired` only after a real "yo" is seen crossing the cutoff in the logs.

### Calibration round 1 — "no yellow" diagnosed (2026-06-18)

User flashed/ran and saw **no yellow on "yo".** A 4-lens adversarially-verified
review (+ the device's own serial logs) found:

- **Current symptom = stale STUB firmware.** The booted binary was compiled BEFORE
  the microfrontend was vendored (`MWW_HAVE_FRONTEND=0`), so `mww_process()`
  returns −1 every window and `mww_step()` early-returns before logging or
  `led.yellow()`. Proof: device logs show many `[voice] sound gate open` lines but
  **zero** `[voice] mww prob=` lines (`mww_init rc=0`, model loads, but never
  scores). The frontend-enabled ELF was built but **never flashed**. → **reflash**
  (checklist below). Yellow is impossible until then, regardless of how clearly you
  say "yo".
- **REAL BUG fixed — feature scale was `/64`, should be `/25.6`.** The model's int8
  input scale `0.10196078 = 26/255` exactly ⇒ feature domain `[0, 26]`;
  microWakeWord/pymicro scale the raw uint16 filterbank by `0.0390625 = 1/25.6`
  (raw max ~665 → 26). The shim's `kFeatScale = 64·0.10196 (6.525)` was ~2.5× too
  large, compressing every feature toward the −128 floor (a saturating raw 665
  landed at int8 **−26** instead of **+127**) — starving the model so prob never
  rose. Fixed to `kFeatScale = 25.6·0.10196 (~2.610)` in `mww.cc` (ESPHome uses the
  same `666/256 ≈ 2.601`). Masked until now by the stub.
- **NOT the cause — sliding-window averaging.** `yo.json` has
  `sliding_window_size:5`, but the device thresholds the per-window MAX, and
  `mean(N) ≤ max(N)`, so averaging would only RAISE the bar. It's a false-accept
  knob for later, not a no-yellow fix.
- **Inherent risk remains — "yo" is 2 phonemes.** Even correctly scaled the model
  may not hit a raw single-window 0.90. If post-reflash `max_prob` lands ~0.5–0.85,
  lower `WAKE_PROB_CUTOFF` (voice.rs) toward ~0.83 (per `yo.json`) rather than
  averaging — or move to a longer phrase ("hey listam").

**Diagnostic added — one-line per-utterance WAKE SUMMARY.** `mww.cc` now tracks
`g_invokes` + `g_max_prob` (getters `mww_last_invokes()`/`mww_last_prob()`, declared
in `mww.h`, linked in stub builds too); `voice.rs` logs at END:
`[voice] WAKE SUMMARY windows=.. invokes=.. max_prob=.. max_feat_peak=.. fired=.. (cutoff=..)`.
Read it: `invokes=0` ⇒ frontend not running (stub/starved → reflash);
`invokes≥1` with `max_prob` in `0..0.9` ⇒ model ran, scale/cutoff tune (not the
front-end); `max_prob≥cutoff` ⇒ yellow fired.

**Device checklist (one "say yo" pass is now conclusive):**
1. `lsof /dev/cu.usbmodem*` must be empty (a parallel session bricks mid-flash).
2. Flash the newest ELF: `espflash flash --monitor --partition-table partitions.csv
   target/xtensa-esp32s3-espidf/release/leaf-esp32` (or `cargo run --release`).
3. Boot: expect `[voice] mww_init rc=0 (62304 byte model)`.
4. Say "yo" once; read the `[voice] mww prob=…` lines and the `WAKE SUMMARY`.
   - `invokes=0` → flash didn't take / still stub.
   - `max_feat_peak` healthy (hundreds, host smoke ~665) + `max_prob` < 0.90 →
     scale was the issue (now fixed) or `yo` is just hard → tune the cutoff.
   - `max_prob ≥ 0.90` → `wake FIRED`, LED yellow. Then flip streaming to gate on
     `wake_fired`.

### Calibration round 2 — IT FIRES (2026-06-18, on real ESP32-S3)

After reflashing the frontend ELF with the `/25.6` scale fix, a live "say yo" pass
(monitored over USB-JTAG via DTR-asserted pyserial, `/tmp/dtrcap.py`):

- **`wake FIRED` 10× at `max_prob=0.996`** on clear "yo" near the mic → LED yellow.
  The on-device wake works end-to-end.
- `feat_peak` ~500–655 on every utterance (fired or not) → the scale fix is right;
  features land in the model's expected range regardless of outcome.
- **Clean separation:** fires ≥0.98, all non-fires ≤0.46 — the 0.90 cutoff sits in
  an empty band, so no borderline jitter. Lowering it wouldn't help (the misses
  score 0.03–0.17, not 0.5–0.9).
- **The model rejects ambient correctly:** non-"yo" sounds that trip the −30 dBFS
  gate score ~0.03 → no fire. This is the false-wake suppression the model was
  added for, now demonstrated.
- **Soft spot = recall:** crisp/near "yo" fires reliably; mumbled/distant "yo" can
  peak ~0.2–0.46 and miss — the inherent 2-phoneme limitation, not a defect.

**Status:** Stage 5 on-device wake is FUNCTIONAL. The original "yellow only on wake"
ask is met. Open product choices (not bugs):
1. **Flip streaming to gate on `wake_fired`** (deferred in round 1) — now that the
   gate is trusted and cleanly separated. Trade-off: rejects the ambient false-
   streams, but a non-crisp "yo" before a command would drop that command. Pairs
   well with a short refractory + the host whisper re-confirm.
2. **Lift recall** without changing the word: add ~20–50 of the user's own "yo"
   clips to training (plan §Data sourcing) — "significantly improves detection".
3. **Switch to "hey listam"** (plan's standing #1 rec) for rock-solid recall if
   spotty "yo" recall is unacceptable.

### Sensitivity + training-dataset capture (2026-06-18)

Chosen direction: improve recall by collecting the user's own real-life "yo" data
(for a future personal-voice retrain), plus make the gate easier to trigger.

- **dB gate ≥30% more sensitive:** `wake_db_threshold −30 → −34` dBFS (cfg.toml,
  ~37%/4 dB more amplitude sensitivity, still 3 dB above the ~−37 ambient floor so
  silence-detection still ends utterances). Now SAFE because the on-device model
  rejects the ambient sounds the looser gate lets through (ambient scores ~0.03).
- **Leaf labels each utterance:** `F_END` payload extended from `[reason]` to
  `[reason, fired:u8, probMilli:u16le, featPeak:u16le]` (voice.rs, from the values
  already read for WAKE SUMMARY). Backward compatible.
- **Dataset capture (shared backend + headless):** `audio-bridge.mjs` decodes the
  optional label tail → `voice-bridge.mjs` attaches `utterance.wake` → new
  `voice-dataset.mjs` (`createUtteranceDataset`) writes every received utterance as
  a timestamped **WAV + JSON sidecar**, auto-named `…_fired-yes/no/unk_p<milli>_…`
  (positives that fired the wake AND hard-negatives that only tripped the gate —
  both are training gold). Pruned past `maxFiles`. Wired into the **headless**
  `service.mjs` (the always-on audio sink); off with `LISTAM_VOICE_DATASET=0`, dir
  via `LISTAM_VOICE_DATASET_DIR` (default `<storageDir>/voice-dataset`). 16 backend
  Node tests green.
- **Architecture note:** only the headless runs the audio bridge, so the raw "yo"
  corpus accumulates there (always-on = ideal collector). The desktop receives the
  resulting list items via hypercore, not the audio — to also collect on the
  desktop it would need to run `startAudioBridge` + be in the leaf's `audio_addr`.
- **Retrain loop:** once enough clips accumulate, feed the `fired-yes` WAVs (+ a
  sample of `fired-no` hard-negatives) into the microWakeWord trainer alongside the
  synthetic positives (plan §Data sourcing: "~20–50 personal clips significantly
  improves detection") and re-export the int8 model.
