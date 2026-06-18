# mww: C-ABI shim over TFLite-Micro for the microWakeWord "yo" model, PLUS the
# vendored TensorFlow audio microfrontend (the exact code pymicro-features wraps,
# so on-device features match the training frontend bit-for-bit).
#
# Modern esp-tflite-micro dropped the experimental microfrontend C lib, so we
# vendor it here under tensorflow/lite/experimental/microfrontend/lib/ + pymicro's
# fixed-point kissfft under kissfft/. The `.` include dir makes the canonical
# `tensorflow/lite/experimental/microfrontend/lib/frontend.h` resolve, which flips
# mww.cc's __has_include guard on and enables real mww_process().
#
# kissfft note: esp-tflite-micro's signal lib also vendors kissfft but in the
# `kiss_fft_fixed16` namespace and only as `.c`; pymicro's wrapper uses the
# `kissfft_fixed16` namespace and #includes the `.cc` — distinct symbols + distinct
# filenames, so no collision and no ambiguous resolution against esp's copy.

set(frontend_dir "tensorflow/lite/experimental/microfrontend/lib")

# Exactly the 16 sources pymicro-features' setup.py compiles. The two standalone
# kissfft sources it also lists are redundant here: kiss_fft_int16.cc #includes
# kiss_fft.cc / tools/kiss_fftr.cc into the kissfft_fixed16 namespace, which is the
# only kissfft resolution the frontend uses. Compiling them standalone too would
# only add unused global-scope symbols.
set(frontend_srcs
    "${frontend_dir}/kiss_fft_int16.cc"
    "${frontend_dir}/fft.cc"
    "${frontend_dir}/fft_util.cc"
    "${frontend_dir}/filterbank.cc"
    "${frontend_dir}/filterbank_util.cc"
    "${frontend_dir}/frontend.cc"
    "${frontend_dir}/frontend_util.cc"
    "${frontend_dir}/log_lut.cc"
    "${frontend_dir}/log_scale.cc"
    "${frontend_dir}/log_scale_util.cc"
    "${frontend_dir}/noise_reduction.cc"
    "${frontend_dir}/noise_reduction_util.cc"
    "${frontend_dir}/pcan_gain_control.cc"
    "${frontend_dir}/pcan_gain_control_util.cc"
    "${frontend_dir}/window.cc"
    "${frontend_dir}/window_util.cc")

idf_component_register(
    SRCS "mww.cc" ${frontend_srcs}
    # "." → vendored tensorflow/... headers (incl. frontend.h for the guard).
    # "kissfft" → the wrapper's #include "kiss_fft.h" / "kiss_fft.cc" / "tools/...".
    INCLUDE_DIRS "." "kissfft"
    REQUIRES esp-tflite-micro
)

# TFLite-Micro + the vendored frontend need C++17 and are exception/RTTI-free.
target_compile_options(${COMPONENT_LIB} PRIVATE -std=gnu++17 -fno-exceptions -fno-rtti)

# Reduce the level of paranoia to compile the vendored TF / kissfft sources, the
# same way esp-tflite-micro's own CMakeLists does for its TF sources.
target_compile_options(${COMPONENT_LIB} PRIVATE
    -Wno-error=attributes
    -Wno-error=shadow
    -Wno-error=sign-compare
    -Wno-error=double-promotion
    -Wno-maybe-uninitialized
    -Wno-missing-field-initializers
    -Wno-unused-parameter
    -Wno-type-limits)
