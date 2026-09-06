#!/usr/bin/env python3
"""Evaluate streaming microWakeWord TFLite models against labeled WAV clips."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
from microwakeword.inference import Model
from scipy.io import wavfile


def score(
    model_path: Path, audio: np.ndarray, window: int
) -> tuple[float, float, float]:
    # A new Model resets the recurrent state between independent recordings.
    probabilities = np.asarray(Model(str(model_path)).predict_clip(audio), dtype=float)
    if probabilities.size == 0:
        return 0.0, 0.0, 0.0
    if probabilities.size < window:
        rolling = probabilities
    else:
        rolling = np.convolve(probabilities, np.ones(window) / window, mode="valid")
    # The Leaf feeds roughly two 30 ms model inferences per 64 ms PCM block and
    # requires two adjacent blocks to pass. Estimate the highest threshold that
    # satisfies that device-side rule.
    block_scores: dict[int, float] = {}
    for index, probability in enumerate(rolling):
        inference_index = index + max(0, window - 1)
        block = (inference_index * 30) // 64
        block_scores[block] = max(block_scores.get(block, 0.0), float(probability))
    ordered = [block_scores[index] for index in sorted(block_scores)]
    sustained = max(
        (min(left, right) for left, right in zip(ordered, ordered[1:])),
        default=0.0,
    )
    return float(probabilities.max()), float(rolling.max()), sustained


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("dataset", type=Path)
    parser.add_argument("models", nargs="+", type=Path)
    parser.add_argument("--window", type=int, default=5)
    parser.add_argument(
        "--prepad-ms",
        type=int,
        default=0,
        help="prepend silence to simulate the Leaf's pre-roll before the sound gate",
    )
    args = parser.parse_args()

    wavs = sorted(args.dataset.glob("petito_*.wav"))
    if not wavs:
        raise SystemExit(f"No petito_*.wav files found in {args.dataset}")

    rows: list[dict[str, object]] = []
    for wav_path in wavs:
        metadata = json.loads(wav_path.with_suffix(".json").read_text())
        sample_rate, audio = wavfile.read(wav_path)
        if sample_rate != 16_000 or audio.dtype != np.int16 or audio.ndim != 1:
            raise ValueError(f"Unexpected WAV format: {wav_path}")
        if args.prepad_ms:
            prepad = np.zeros(16 * args.prepad_ms, dtype=np.int16)
            audio = np.concatenate((prepad, audio))

        row: dict[str, object] = {
            "sample": metadata["sample"],
            "speaker": metadata.get("speaker", "unknown"),
        }
        for model_path in args.models:
            peak, rolling, sustained = score(model_path, audio, args.window)
            row[f"{model_path.stem}_peak"] = peak
            row[f"{model_path.stem}_rolling"] = rolling
            row[f"{model_path.stem}_sustained"] = sustained
        rows.append(row)

    rows.sort(key=lambda row: int(row["sample"]))
    for row in rows:
        scores = "  ".join(
            f"{model_path.stem}: peak={float(row[f'{model_path.stem}_peak']):.4f} "
            f"avg{args.window}={float(row[f'{model_path.stem}_rolling']):.4f} "
            f"two-block={float(row[f'{model_path.stem}_sustained']):.4f}"
            for model_path in args.models
        )
        print(f"{int(row['sample']):02d} {str(row['speaker']):8s}  {scores}")

    print()
    for model_path in args.models:
        values = np.asarray([
            float(row[f"{model_path.stem}_sustained"]) for row in rows
        ], dtype=float)
        print(
            f"{model_path.stem}: two-block avg{args.window} "
            f"min={values.min():.4f} median={np.median(values):.4f} max={values.max():.4f}"
        )
        for threshold in (
            0.10,
            0.15,
            0.20,
            0.21,
            0.25,
            0.30,
            0.40,
            0.50,
            0.62,
            0.75,
            0.78,
            0.80,
        ):
            print(f"  >= {threshold:.2f}: {int((values >= threshold).sum())}/{len(values)}")


if __name__ == "__main__":
    main()
