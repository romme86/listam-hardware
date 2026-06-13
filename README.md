# listam-hardware

Hardware peers for [Listam](https://github.com/romme86) — a local-first, peer-to-peer shared-list app built on Hypercore/Autobase.

## `leaf-peer/` — Rust mirror replica (host + ESP32-S3)

A "leaf" is a dumb, always-on **blind replica**: it mirrors a project's hypercores over plain TCP and serves them while the real devices are offline, **without holding the encryption key**. It verifies signatures and merkle proofs but never decrypts content.

- **`leaf-host/`** — desktop/server binary (disk or in-memory storage).
- **`leaf-esp32/`** — firmware for the ESP32-S3-N16R8 board: scans WiFi, dials the hub baked into `cfg.toml`, mirrors announced cores to a FAT partition. Provision from `leaf-esp32/cfg.toml.example` (the real `cfg.toml` with WiFi credentials is gitignored).
- **`leaf-core/`** — shared platform-agnostic mirror logic.
- **`vendor/`** — forked hypercore stack with v11 manifest support and ESP-IDF fixes (zero-fill sparse writes for FATFS, ftruncate emulation, a non-panicking handshake socket-error path). Upstreamable.
- **`bridge-js/`** — `e2e.mjs` / `persist-test.mjs` prove the leaf contract against the real app.

The hub side is the TCP leaf bridge in `@listam/backend` (`LISTAM_LEAF_BRIDGE_PORT`), shipped in listam-headless and listam-desktop.

## `esp32-s3-listam-bridge/` — serial-injector prototype (superseded)

An earlier experiment: an ESP32-S3 pushes items into a list over USB serial via a small Node bridge. Kept for reference; `leaf-peer` is the real peer.

## Building the firmware

```bash
source ~/export-esp.sh
cd leaf-peer/leaf-esp32
cp cfg.toml.example cfg.toml   # fill in WiFi + hub control key
cargo build --release
espflash flash --flash-size 16mb --partition-table partitions.csv \
  target/xtensa-esp32s3-espidf/release/leaf-esp32
```
