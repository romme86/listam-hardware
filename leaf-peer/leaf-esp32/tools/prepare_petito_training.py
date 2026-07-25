#!/usr/bin/env python3
"""Prepare personalized microWakeWord features and a training configuration.

Run this from an editable checkout of the official microWakeWord trainer. The
large generated artifacts are intentionally kept below that checkout rather
than committed to the Leaf firmware repository.
"""

from __future__ import annotations

import argparse
import json
import random
from collections.abc import Iterable, Iterator
from pathlib import Path

import numpy as np
import yaml
from mmap_ninja.ragged import RaggedMmap
from microwakeword.audio.audio_utils import (
    generate_features_for_clip,
    remove_silence_webrtc,
)
from microwakeword.audio.augmentation import Augmentation
from scipy.io import wavfile


def load_wav(path: Path) -> np.ndarray:
    sample_rate, audio = wavfile.read(path)
    if sample_rate != 16_000 or audio.ndim != 1:
        raise ValueError(f"Expected mono 16 kHz WAV: {path}")
    if audio.dtype == np.int16:
        return audio.astype(np.float32) / 32768.0
    return audio.astype(np.float32)


def slide_spectrogram(features: np.ndarray, frames: int) -> Iterator[np.ndarray]:
    if frames <= 1:
        yield features
        return
    length = features.shape[0] - frames + 1
    windows = np.lib.stride_tricks.sliding_window_view(
        features, window_shape=(length, features.shape[1])
    )
    for index in range(frames):
        yield np.squeeze(windows[index])


def feature_generator(
    paths: Iterable[Path],
    *,
    repeat: int,
    slide_frames: int,
    augmenter: Augmentation | None,
    trim_speech: bool,
) -> Iterator[np.ndarray]:
    paths = list(paths)
    for _ in range(repeat):
        for path in paths:
            audio = load_wav(path)
            if trim_speech:
                audio = remove_silence_webrtc(audio)
            if augmenter is not None:
                audio = augmenter.augment_clip(audio)
            features = generate_features_for_clip(audio, step_ms=10)
            yield from slide_spectrogram(features, slide_frames)


def write_mmap(
    destination: Path,
    paths: list[Path],
    *,
    repeat: int,
    slide_frames: int,
    augmenter: Augmentation | None,
    trim_speech: bool,
) -> None:
    if destination.exists():
        print(f"Keeping existing feature mmap: {destination}")
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    RaggedMmap.from_generator(
        out_dir=str(destination),
        sample_generator=feature_generator(
            paths,
            repeat=repeat,
            slide_frames=slide_frames,
            augmenter=augmenter,
            trim_speech=trim_speech,
        ),
        batch_size=100,
        verbose=True,
    )


def split_synthetic(paths: list[Path]) -> dict[str, list[Path]]:
    random.Random(20260725).shuffle(paths)
    train_end = int(len(paths) * 0.80)
    validation_end = int(len(paths) * 0.90)
    return {
        "training": paths[:train_end],
        "validation": paths[train_end:validation_end],
        "testing": paths[validation_end:],
    }


def real_splits(dataset: Path) -> dict[str, list[Path]]:
    split = json.loads((dataset / "training-split.json").read_text())
    result: dict[str, list[Path]] = {}
    for output_name, input_name in (
        ("training", "train"),
        ("validation", "validation"),
        ("testing", "test"),
    ):
        result[output_name] = [
            dataset / f"petito_{sample:03d}.wav"
            for sample in split[input_name]["samples"]
        ]

    # The corpus is small and speaker 1 has many more clips. Repeat path entries
    # from the under-represented speakers so each voice contributes roughly the
    # same number of augmented training examples.
    by_speaker: dict[str, list[Path]] = {}
    for path in result["training"]:
        metadata = json.loads(path.with_suffix(".json").read_text())
        by_speaker.setdefault(metadata["speaker"], []).append(path)
    target = max(len(paths) for paths in by_speaker.values())
    balanced: list[Path] = []
    for paths in by_speaker.values():
        copies = (target + len(paths) - 1) // len(paths)
        balanced.extend((paths * copies)[:target])
    result["training"] = balanced
    return result


def make_augmenter(*, real: bool) -> Augmentation:
    probabilities = {
        "SevenBandParametricEQ": 0.15,
        "TanhDistortion": 0.10,
        "PitchShift": 0.15,
        "BandStopFilter": 0.10,
        "AddColorNoise": 0.35,
        "AddBackgroundNoise": 0.0,
        "Gain": 1.0,
        "GainTransition": 0.15,
        "RIR": 0.0,
    }
    return Augmentation(
        augmentation_duration_s=3.2,
        augmentation_probabilities=probabilities,
        color_min_snr_db=8,
        color_max_snr_db=30,
        min_gain_db=-12 if real else -30,
        max_gain_db=3 if real else 0,
        min_jitter_s=0.10,
        max_jitter_s=0.30,
    )


def write_config(output: Path, feature_root: Path, negative_root: Path) -> Path:
    config = {
        "window_step_ms": 10,
        "train_dir": str(output / "trained_model"),
        "features": [
            {
                "features_dir": str(feature_root / "synthetic"),
                "sampling_weight": 4.0,
                "penalty_weight": 1.0,
                "truth": True,
                "truncation_strategy": "truncate_start",
                "type": "mmap",
            },
            {
                "features_dir": str(feature_root / "real"),
                "sampling_weight": 6.0,
                "penalty_weight": 2.0,
                "truth": True,
                "truncation_strategy": "truncate_start",
                "type": "mmap",
            },
            {
                "features_dir": str(negative_root / "speech"),
                "sampling_weight": 10.0,
                "penalty_weight": 1.0,
                "truth": False,
                "truncation_strategy": "random",
                "type": "mmap",
            },
            {
                "features_dir": str(negative_root / "dinner_party"),
                "sampling_weight": 10.0,
                "penalty_weight": 1.0,
                "truth": False,
                "truncation_strategy": "random",
                "type": "mmap",
            },
            {
                "features_dir": str(negative_root / "no_speech"),
                "sampling_weight": 5.0,
                "penalty_weight": 1.0,
                "truth": False,
                "truncation_strategy": "random",
                "type": "mmap",
            },
            {
                "features_dir": str(negative_root / "dinner_party_eval"),
                "sampling_weight": 0.0,
                "penalty_weight": 1.0,
                "truth": False,
                "truncation_strategy": "split",
                "type": "mmap",
            },
        ],
        "training_steps": [12_000, 4_000],
        "positive_class_weight": [1, 1],
        "negative_class_weight": [20, 30],
        "learning_rates": [0.001, 0.0002],
        "batch_size": 128,
        "time_mask_max_size": [0, 0],
        "time_mask_count": [0, 0],
        "freq_mask_max_size": [0, 0],
        "freq_mask_count": [0, 0],
        "eval_step_interval": 500,
        "clip_duration_ms": 1500,
        "target_minimization": 0.5,
        "minimization_metric": "ambient_false_positives_per_hour",
        "maximization_metric": "average_viable_recall",
    }
    config_path = output / "training_parameters.yaml"
    output.mkdir(parents=True, exist_ok=True)
    config_path.write_text(yaml.safe_dump(config, sort_keys=False))
    return config_path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--leaf-root", required=True, type=Path)
    parser.add_argument("--synthetic-root", required=True, type=Path)
    parser.add_argument("--negative-root", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    dataset = (
        args.leaf_root
        / "wakeword-recordings"
        / "petito-20260725-173229"
    )
    synthetic_paths = sorted(args.synthetic_root.glob("**/*.wav"))
    if len(synthetic_paths) < 1_000:
        raise SystemExit(f"Only {len(synthetic_paths)} synthetic WAVs found")

    feature_root = args.output / "features"
    synthetic = split_synthetic(synthetic_paths)
    real = real_splits(dataset)
    synthetic_augmenter = make_augmenter(real=False)
    real_augmenter = make_augmenter(real=True)

    for split_name, paths in synthetic.items():
        write_mmap(
            feature_root / "synthetic" / split_name / "petito_synthetic_mmap",
            paths,
            repeat=1,
            slide_frames=10 if split_name != "testing" else 1,
            augmenter=synthetic_augmenter,
            trim_speech=False,
        )

    for split_name, paths in real.items():
        training = split_name == "training"
        write_mmap(
            feature_root / "real" / split_name / "petito_real_mmap",
            paths,
            repeat=80 if training else 1,
            slide_frames=10 if training else 1,
            augmenter=real_augmenter if training else None,
            trim_speech=True,
        )

    config_path = write_config(args.output, feature_root, args.negative_root)
    print(f"Prepared {len(synthetic_paths)} synthetic and 29 real samples")
    print(config_path)


if __name__ == "__main__":
    main()
