#!/usr/bin/env python3
"""Import isolated Petito utterances from one longer recording.

The source may be any format supported by libsndfile (including MP3). Each
selected interval is converted to mono 16 kHz PCM16, peak-normalized, and
padded or cropped to the 1.8 second format used by the Leaf capture corpus.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import soundfile as sf
from scipy.io import wavfile
from scipy.signal import resample_poly


SAMPLE_RATE = 16_000
CLIP_SECONDS = 1.8
TARGET_PEAK_DBFS = -3.0


def parse_interval(value: str) -> tuple[float, float]:
    try:
        start_text, end_text = value.split(":", maxsplit=1)
        start, end = float(start_text), float(end_text)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected START:END in seconds") from error
    if start < 0 or end <= start:
        raise argparse.ArgumentTypeError("interval must satisfy 0 <= START < END")
    return start, end


def convert_interval(
    audio: np.ndarray,
    source_rate: int,
    interval: tuple[float, float],
) -> np.ndarray:
    start, end = interval
    selected = audio[round(start * source_rate) : round(end * source_rate)]
    if selected.size == 0:
        raise ValueError(f"Empty interval {start}:{end}")

    converted = resample_poly(selected, SAMPLE_RATE, source_rate)
    target_samples = round(CLIP_SECONDS * SAMPLE_RATE)
    if converted.size < target_samples:
        converted = np.pad(converted, (0, target_samples - converted.size))
    else:
        converted = converted[:target_samples]

    peak = float(np.max(np.abs(converted)))
    if peak <= 1e-9:
        raise ValueError(f"Silent interval {start}:{end}")
    target_peak = 10 ** (TARGET_PEAK_DBFS / 20)
    converted = np.clip(converted * (target_peak / peak), -1.0, 1.0)
    return np.round(converted * 32767).astype(np.int16)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=Path)
    parser.add_argument("dataset", type=Path)
    parser.add_argument("--speaker", required=True)
    parser.add_argument("--first-sample", required=True, type=int)
    parser.add_argument(
        "--interval",
        action="append",
        required=True,
        type=parse_interval,
        help="START:END in seconds; repeat once per utterance",
    )
    args = parser.parse_args()

    audio, source_rate = sf.read(args.source, always_2d=True, dtype="float32")
    mono = audio.mean(axis=1)
    args.dataset.mkdir(parents=True, exist_ok=True)

    for offset, interval in enumerate(args.interval):
        sample = args.first_sample + offset
        stem = f"petito_{sample:03d}"
        wav_path = args.dataset / f"{stem}.wav"
        metadata_path = args.dataset / f"{stem}.json"
        if wav_path.exists() or metadata_path.exists():
            raise FileExistsError(f"Refusing to replace existing sample {sample}")

        converted = convert_interval(mono, source_rate, interval)
        wavfile.write(wav_path, SAMPLE_RATE, converted)
        metadata = {
            "sample": sample,
            "label": "petito",
            "speaker": args.speaker,
            "source": args.source.name,
            "source_interval_seconds": list(interval),
            "sample_rate": SAMPLE_RATE,
            "duration_seconds": CLIP_SECONDS,
            "normalization_peak_dbfs": TARGET_PEAK_DBFS,
        }
        metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")
        print(f"Wrote {wav_path} from {interval[0]:.2f}:{interval[1]:.2f}")


if __name__ == "__main__":
    main()
