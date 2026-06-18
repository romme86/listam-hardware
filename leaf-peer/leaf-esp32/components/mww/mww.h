#pragma once
/* C-ABI shim over TFLite-Micro for the microWakeWord "yo" model.
 * Bound into Rust via esp-idf-sys `bindings_module = "mww"`. Keep this header
 * pure C so bindgen handles it (the .cc side is C++ / TFLite-Micro). */
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Initialize the int8 streaming interpreter (model blob) + the audio
 * microfrontend (matched to the pymicro-features training frontend:
 * 30ms window, 10ms step, 40 channels, 125-7500Hz, PCAN). 0 ok, <0 error. */
int mww_init(const uint8_t *model_data, int model_len);

/* Reset streaming state (model resource variables + frontend) — call when
 * starting a fresh detection window (e.g. on dB-gate open) and after a fire. */
void mww_reset(void);

/* Feed 16kHz mono int16 PCM. Runs the microfrontend over completed 10ms frames
 * and the streaming model every 3 frames. Returns the MAX wake probability
 * (0..1) produced during this call, or -1 on error. The model carries its own
 * streaming state across calls. */
float mww_process(const int16_t *pcm, int num_samples);

/* Last raw uint16 microfrontend peak (debug/calibration of the feature scale). */
uint16_t mww_last_feat_peak(void);

/* Diagnostics for one detection window (since the last mww_reset): the number of
 * model Invoke()s (0 ⇒ the frontend produced no full 3-frame group — stub/starved)
 * and the max wake probability seen. Make one spoken "yo" conclusive. */
int mww_last_invokes(void);
float mww_last_prob(void);

#ifdef __cplusplus
}
#endif
