/* TFLite-Micro shim for the microWakeWord "yo" int8 streaming model + the audio
 * microfrontend (matched to pymicro-features). 13 model ops incl. the streaming
 * resource-variable ops. Arena in PSRAM (this leaf is internal-RAM constrained).
 *
 * NOTE: modern esp-tflite-micro removed the experimental microfrontend C lib
 * (frontend.h) in favor of an audio-preprocessor tflite model + the signal/ DSP
 * lib. We guard on frontend.h: vendor the microfrontend source into this
 * component (the exact code pymicro-features wraps -> guaranteed feature match)
 * to enable real on-device feature extraction. Until then mww_process() stubs
 * out (the model still loads + validates; wake inference is pending). */
#include "mww.h"

#include "tensorflow/lite/micro/micro_interpreter.h"
#include "tensorflow/lite/micro/micro_mutable_op_resolver.h"
#include "tensorflow/lite/micro/micro_allocator.h"
#include "tensorflow/lite/micro/micro_resource_variable.h"
#include "tensorflow/lite/schema/schema_generated.h"
#include "esp_heap_caps.h"

#if defined(__has_include) && \
    __has_include("tensorflow/lite/experimental/microfrontend/lib/frontend.h")
#include "tensorflow/lite/experimental/microfrontend/lib/frontend.h"
#include "tensorflow/lite/experimental/microfrontend/lib/frontend_util.h"
#define MWW_HAVE_FRONTEND 1
#else
#define MWW_HAVE_FRONTEND 0
#endif

#include <cstring>
#include <cmath>

namespace {
constexpr int kArenaSize = 64 * 1024;
constexpr int kFeaturesPerFrame = 40;
constexpr int kFramesPerInference = 3;     // model input is [1,3,40]
// pymicro-features / microWakeWord scale the raw uint16 filterbank feature by
// 0.0390625 (= 1/25.6) to get the model's float feature; the model's int8 input
// quant is scale 0.10196078 (= 26/255 exactly → feature domain [0, 26]), zero-
// point -128. Fold both:
//   int8 = round(raw_uint16 / 25.6 / 0.10196078) - 128
// (A previous /64 here under-scaled features ~2.5x toward the -128 floor — a loud
// channel that should saturate int8 +127 landed near -26 — starving the model so
// prob never crossed the cutoff. ESPHome's micro_wake_word uses the same ~2.601
// divisor, 666/256.)
constexpr float kFeatScale = 25.6f * 0.10196078431f;  // ~2.610; raw/this - 128

uint8_t *g_arena = nullptr;
const tflite::Model *g_model = nullptr;
tflite::MicroInterpreter *g_interpreter = nullptr;
using MwwOpResolver = tflite::MicroMutableOpResolver<13>;
MwwOpResolver g_resolver;

bool g_fe_ready = false;
int8_t g_frame_buf[kFramesPerInference * kFeaturesPerFrame];
int g_frames_collected = 0;
uint16_t g_last_peak = 0;
int g_invokes = 0;         // model Invoke() count since reset (0 ⇒ frontend stub / starved)
float g_max_prob = -1.0f;  // max wake probability seen since reset

#if MWW_HAVE_FRONTEND
FrontendConfig g_fe_config;
FrontendState g_fe_state;

inline int8_t quantize_feature(uint16_t raw) {
  long q = lround((float)raw / kFeatScale) - 128;
  if (q < -128) q = -128;
  if (q > 127) q = 127;
  return (int8_t)q;
}

// Feed one completed 40-channel frame; run inference when 3 are collected.
float push_frame(const uint16_t *values) {
  for (int c = 0; c < kFeaturesPerFrame; ++c) {
    if (values[c] > g_last_peak) g_last_peak = values[c];
    g_frame_buf[g_frames_collected * kFeaturesPerFrame + c] = quantize_feature(values[c]);
  }
  g_frames_collected++;
  if (g_frames_collected < kFramesPerInference) return -1.0f;
  g_frames_collected = 0;
  TfLiteTensor *input = g_interpreter->input(0);
  std::memcpy(input->data.int8, g_frame_buf, sizeof(g_frame_buf));
  g_invokes++;
  if (g_interpreter->Invoke() != kTfLiteOk) return -1.0f;
  TfLiteTensor *output = g_interpreter->output(0);
  float prob = (float)output->data.uint8[0] / 256.0f;  // uint8 scale 1/256
  if (prob > g_max_prob) g_max_prob = prob;
  return prob;
}
#endif  // MWW_HAVE_FRONTEND
}  // namespace

extern "C" int mww_init(const uint8_t *model_data, int model_len) {
  (void)model_len;
  g_model = tflite::GetModel(model_data);
  if (g_model->version() != TFLITE_SCHEMA_VERSION) return -1;

  if (g_resolver.AddConv2D() != kTfLiteOk) return -10;
  if (g_resolver.AddDepthwiseConv2D() != kTfLiteOk) return -11;
  if (g_resolver.AddFullyConnected() != kTfLiteOk) return -12;
  if (g_resolver.AddLogistic() != kTfLiteOk) return -13;
  if (g_resolver.AddQuantize() != kTfLiteOk) return -14;
  if (g_resolver.AddConcatenation() != kTfLiteOk) return -15;
  if (g_resolver.AddReshape() != kTfLiteOk) return -16;
  if (g_resolver.AddSplitV() != kTfLiteOk) return -17;
  if (g_resolver.AddStridedSlice() != kTfLiteOk) return -18;
  if (g_resolver.AddVarHandle() != kTfLiteOk) return -19;
  if (g_resolver.AddReadVariable() != kTfLiteOk) return -20;
  if (g_resolver.AddAssignVariable() != kTfLiteOk) return -21;
  if (g_resolver.AddCallOnce() != kTfLiteOk) return -22;

  if (g_arena == nullptr) {
    g_arena = (uint8_t *)heap_caps_malloc(kArenaSize, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    if (g_arena == nullptr) return -4;
  }
  tflite::MicroAllocator *allocator = tflite::MicroAllocator::Create(g_arena, kArenaSize);
  if (allocator == nullptr) return -3;
  tflite::MicroResourceVariables *resource_variables =
      tflite::MicroResourceVariables::Create(allocator, 16);

  static tflite::MicroInterpreter interpreter(g_model, g_resolver, allocator, resource_variables);
  g_interpreter = &interpreter;
  if (g_interpreter->AllocateTensors() != kTfLiteOk) {
    g_interpreter = nullptr;
    return -2;
  }

#if MWW_HAVE_FRONTEND
  // Microfrontend matched to the training (pymicro-features) config.
  FrontendFillConfigWithDefaults(&g_fe_config);
  g_fe_config.window.size_ms = 30;
  g_fe_config.window.step_size_ms = 10;
  g_fe_config.filterbank.num_channels = kFeaturesPerFrame;
  g_fe_config.filterbank.lower_band_limit = 125.0f;
  g_fe_config.filterbank.upper_band_limit = 7500.0f;
  g_fe_config.pcan_gain_control.enable_pcan = 1;
  if (!FrontendPopulateState(&g_fe_config, &g_fe_state, 16000)) return -5;
  g_fe_ready = true;
#endif
  g_frames_collected = 0;
  return 0;
}

extern "C" void mww_reset(void) {
#if MWW_HAVE_FRONTEND
  if (g_fe_ready) FrontendReset(&g_fe_state);
#endif
  g_frames_collected = 0;
  g_last_peak = 0;
  g_invokes = 0;
  g_max_prob = -1.0f;
  if (g_interpreter != nullptr) g_interpreter->Reset();
}

extern "C" float mww_process(const int16_t *pcm, int num_samples) {
#if MWW_HAVE_FRONTEND
  if (g_interpreter == nullptr || !g_fe_ready) return -1.0f;
  float max_prob = -1.0f;
  size_t offset = 0;
  while (offset < (size_t)num_samples) {
    size_t read = 0;
    struct FrontendOutput out =
        FrontendProcessSamples(&g_fe_state, pcm + offset, num_samples - offset, &read);
    offset += read;
    if (read == 0) break;
    if (out.size == kFeaturesPerFrame) {
      float p = push_frame(out.values);
      if (p > max_prob) max_prob = p;
    }
  }
  return max_prob;
#else
  (void)pcm;
  (void)num_samples;
  return -1.0f;  // microfrontend not vendored yet
#endif
}

extern "C" uint16_t mww_last_feat_peak(void) { return g_last_peak; }

// Calibration/diagnostic getters — defined outside the frontend #if so they link
// in stub builds too. invokes==0 after an utterance ⇒ the frontend never produced
// a full 3-frame group (stub or starved); prob is the max wake probability seen.
extern "C" int mww_last_invokes(void) { return g_invokes; }
extern "C" float mww_last_prob(void) { return g_max_prob; }
