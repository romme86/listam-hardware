#!/usr/bin/env python3
"""Receive labeled PCM clips from the Leaf wake-word capture firmware."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import select
import socket
import struct
import sys
import time
import wave
from pathlib import Path


HEADER = struct.Struct("<4sIII")
MAGIC = b"PET1"
ACK = b"\x06"


def recv_exact(conn: socket.socket, size: int) -> bytes:
    chunks: list[bytes] = []
    remaining = size
    while remaining:
        chunk = conn.recv(remaining)
        if not chunk:
            raise ConnectionError("Leaf disconnected during a clip")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def level_stats(pcm: bytes) -> tuple[float, float, float]:
    samples = memoryview(pcm).cast("h")
    if not samples:
        return -120.0, -120.0, 0.0
    peak = max(abs(int(sample)) for sample in samples)
    mean_square = sum(int(sample) * int(sample) for sample in samples) / len(samples)
    rms = math.sqrt(mean_square)
    peak_dbfs = 20.0 * math.log10(peak / 32768.0) if peak else -120.0
    rms_dbfs = 20.0 * math.log10(rms / 32768.0) if rms else -120.0
    clipped = sum(abs(int(sample)) >= 32760 for sample in samples) / len(samples)
    return peak_dbfs, rms_dbfs, clipped


def save_clip(directory: Path, number: int, board_sequence: int, sample_rate: int, pcm: bytes) -> Path:
    base = f"petito_{number:03d}"
    wav_path = directory / f"{base}.wav"
    with wave.open(str(wav_path), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(sample_rate)
        wav.writeframes(pcm)

    peak_dbfs, rms_dbfs, clipped = level_stats(pcm)
    metadata = {
        "label": "petito",
        "sample": number,
        "boardSequence": board_sequence,
        "sampleRate": sample_rate,
        "channels": 1,
        "sampleWidthBytes": 2,
        "micGainShift": 0,
        "durationMs": round(len(pcm) / 2 / sample_rate * 1000),
        "peakDbfs": round(peak_dbfs, 2),
        "rmsDbfs": round(rms_dbfs, 2),
        "clippedFraction": round(clipped, 6),
        "speaker": None,
        "capturedAt": dt.datetime.now(dt.timezone.utc).isoformat(),
        "sha256": hashlib.sha256(pcm).hexdigest(),
    }
    wav_path.with_suffix(".json").write_text(json.dumps(metadata, indent=2) + "\n")
    return wav_path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=10001)
    parser.add_argument("--count", type=int, default=60)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    directory = args.output or Path("wakeword-recordings") / f"petito-{stamp}"
    directory.mkdir(parents=True, exist_ok=True)

    accepted = len(list(directory.glob("petito_*.wav")))
    known_pcm_hashes: set[str] = set()
    for metadata_path in directory.glob("petito_*.json"):
        try:
            metadata = json.loads(metadata_path.read_text())
            if metadata.get("sha256"):
                known_pcm_hashes.add(metadata["sha256"])
        except (OSError, ValueError):
            pass
    print(f"CAPTURE_DIR={directory.resolve()}", flush=True)
    print(f"LISTENING={args.host}:{args.port} TARGET={args.count}", flush=True)

    with socket.create_server((args.host, args.port), reuse_port=False) as server:
        server.settimeout(1.0)
        while accepted < args.count:
            try:
                conn, address = server.accept()
            except TimeoutError:
                continue
            print(f"LEAF_CONNECTED={address[0]}:{address[1]}", flush=True)
            print("CONTROL=type 'record' or 'batch N'", flush=True)
            with conn:
                # People may take minutes to switch speakers between samples.
                # Keep the accepted connection idle indefinitely; the Leaf
                # reconnects and resends a clip if an actual I/O error occurs.
                conn.settimeout(None)
                pending_captures = 0
                trigger_in_flight = False
                while accepted < args.count:
                    if pending_captures > 0 and not trigger_in_flight:
                        try:
                            conn.sendall(b"R")
                        except OSError as error:
                            print(f"TRIGGER_FAILED={error}", flush=True)
                            break
                        pending_captures -= 1
                        trigger_in_flight = True
                        print(f"TRIGGER_SENT=PENDING_{pending_captures}", flush=True)

                    readable, _, _ = select.select([conn, sys.stdin], [], [], 1.0)
                    if sys.stdin in readable:
                        command = sys.stdin.readline().strip().lower()
                        if command == "record":
                            pending_captures += 1
                        elif command.startswith("batch "):
                            try:
                                requested = int(command.split(maxsplit=1)[1])
                                if not 1 <= requested <= 20:
                                    raise ValueError
                                pending_captures += requested
                            except ValueError:
                                print("CONTROL_ERROR=batch size must be 1..20", flush=True)
                        elif command:
                            print("CONTROL_ERROR=use 'record' or 'batch N'", flush=True)
                    if conn not in readable:
                        continue

                    try:
                        header = recv_exact(conn, HEADER.size)
                        magic, board_sequence, sample_rate, sample_count = HEADER.unpack(header)
                        if magic != MAGIC:
                            raise ValueError(f"unexpected magic {magic!r}")
                        if sample_rate != 16_000 or not 8_000 <= sample_count <= 64_000:
                            raise ValueError(f"invalid clip shape rate={sample_rate} samples={sample_count}")
                        pcm = recv_exact(conn, sample_count * 2)
                    except (ConnectionError, OSError, ValueError) as error:
                        print(f"LEAF_DISCONNECTED={error}", flush=True)
                        break

                    pcm_hash = hashlib.sha256(pcm).hexdigest()
                    if pcm_hash in known_pcm_hashes:
                        print(
                            f"DUPLICATE_RETRY=board-{board_sequence} HASH={pcm_hash[:12]} (acknowledging, not saving)",
                            flush=True,
                        )
                        try:
                            conn.sendall(ACK)
                        except OSError as error:
                            print(f"ACK_FAILED={error}", flush=True)
                            break
                        trigger_in_flight = False
                        continue

                    accepted += 1
                    wav_path = save_clip(directory, accepted, board_sequence, sample_rate, pcm)
                    known_pcm_hashes.add(pcm_hash)
                    peak_dbfs, rms_dbfs, clipped = level_stats(pcm)
                    quality = "OK"
                    if peak_dbfs < -30:
                        quality = "QUIET"
                    elif clipped > 0.01:
                        quality = "CLIPPED"
                    print(
                        f"SAVED={accepted:02d}/{args.count} QUALITY={quality} "
                        f"PEAK={peak_dbfs:.1f}dB RMS={rms_dbfs:.1f}dB FILE={wav_path.name}",
                        flush=True,
                    )
                    try:
                        conn.sendall(ACK)
                    except OSError as error:
                        # The WAV is already durable. A reconnect/resend will be
                        # recognized by its PCM hash and acknowledged without
                        # creating another numbered training sample.
                        print(f"ACK_FAILED={error}", flush=True)
                        break
                    trigger_in_flight = False
                    if pending_captures > 0:
                        time.sleep(1.5)

    print(f"COMPLETE={accepted}/{args.count} DIR={directory.resolve()}", flush=True)


if __name__ == "__main__":
    main()
