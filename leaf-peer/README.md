# listam leaf peer

A **leaf** is a dumb, always-on replica of a listam project: it mirrors the
project's hypercores (writer oplogs, autobase bootstrap, system core) over a
plain TCP connection and serves them back to any peer — including while every
real device is offline. It never holds the encryption key and cannot read
list content: it stores and forwards ciphertext with full merkle/signature
verification (the same guarantee Holepunch blind peers give).

Two builds share the same Rust core (`leaf-core`):

- **`leaf-host`** — desktop/server binary (Mac, Pi, VM). Disk storage.
- **`leaf-esp32`** — ESP32-S3 firmware (tested target: S3-N16R8). RAM
  storage in PSRAM (v1), survives reconnects, loses data on power loss.

This supersedes the earlier `hardware/esp32-s3-listam-bridge` experiment
(USB-serial item injector): a leaf is a real protocol peer, not a sensor.

## How it fits together

```
listam app (desktop / headless / mobile)
   │  @listam/backend lib/leaf-bridge.mjs
   │  - TCP listener, store.replicate(socket) per connection
   │  - "leaf control core": hub-written hypercore announcing
   │    {"add": ["<core key hex>", ...]} for every project core
   ▼
 TCP (LAN / tailscale / WireGuard — payload is Noise-encrypted)
   ▲
   │  leaf (leaf-host or ESP32)
   │  - dials every configured app, speaks the hypercore v10/v11
   │    wire protocol (secret-stream + protomux compatible)
   │  - mirrors the control core, learns project core keys from it,
   │    mirrors everything (download-all), serves requests
```

Provisioning is one value: the **control core key**, printed by the bridge at
startup. Give it to the leaf; everything else is learned in-band.

## Quick start (Mac / host)

```sh
# 1. listam headless app with the bridge enabled
cd listam-headless
LISTAM_LEAF_BRIDGE_PORT=9993 node headless.mjs run --storage ~/listam-data
# → logs: [leaf-bridge] control core key (provision your leaf with this): <hex>

# 2. the leaf
cd hardware/leaf-peer
cargo run -p leaf-host -- \
  --connect 127.0.0.1:9993 \
  --key <control key hex> --control \
  --storage /tmp/leaf-data
```

`--connect` may be repeated (one connection per app: desktop + headless + …).
`--listen <addr>` additionally serves inbound peers (used by the E2E test).
The headless config also accepts `leafBridgePort` (see `src/config.mjs`).

## Which apps can host the bridge

The bridge (`@listam/backend/lib/leaf-bridge.mjs`) is transport-pluggable: the
caller injects a `net`-compatible module so it runs under both Node (headless)
and Bare (desktop/mobile Pear workers, which have no Node `net`). It defaults
to Node's `net`, so headless needs nothing extra.

| App | Runtime | Enable the bridge | Status |
|-----|---------|-------------------|--------|
| **Headless** | Node | `LISTAM_LEAF_BRIDGE_PORT=9993` (or `leafBridgePort` in config) | ✅ shipped |
| **Desktop** | Pear / Bare | `pear run --dev . --leaf-bridge-port 9993` (or `LISTAM_LEAF_BRIDGE_PORT` env). Injects `bare-tcp`. | ✅ shipped, off by default |
| **Mobile** | BareKit worklet | not yet — see `docs/mobile-bridge-plan.md` | ⏳ planned |

The board doesn't care which app hosts the bridge — it dials a TCP `host:port`
and mirrors whatever cores that app announces. A laptop desktop is a fine hub
on the LAN; for always-on availability (and for mobile to benefit), run the
headless hub on a Pi/VM.

**Desktop note:** the bridge binds the port on the machine running the desktop
app; the board dials that machine's LAN IP. Verified end-to-end under Bare
(bare-tcp + corestore replication) against `leaf-host`.

## ESP32-S3 build & flash

**Status: working on hardware.** An S3-N16R8 has mirrored a live listam
project's cores (control + system + autobase cores, verified) over WiFi from a
running headless app — boot → scan → join → dial hub → mirror, no crashes.

Prereqs (installed once): `espup`, `espflash`, `ldproxy`, cmake, ninja, and
the Xtensa toolchain (`espup install --targets esp32s3`), plus
`. ~/export-esp.sh` in the shell.

```sh
cd hardware/leaf-peer/leaf-esp32
cp cfg.toml.example cfg.toml   # wifi (up to 3) + hub_addr + control_key — or leave blank to provision over BLE (see below)
. ~/export-esp.sh
cargo build --release
espflash flash --monitor --flash-size 16mb target/xtensa-esp32s3-espidf/release/leaf-esp32
```

The hub address is the machine running a listam app with the bridge enabled,
reachable from the ESP's WiFi (e.g. `192.168.1.10:9993`).

**Multi-network + hub-aware roaming.** `cfg.toml` holds up to three 2.4GHz
networks (the S3 has no 5GHz radio). The firmware scans, joins the strongest
known one, and — crucially — if **no hub becomes reachable** through it within
~25s, it rotates to the next known network. This handles the common case of a
strong WiFi whose AP blocks client-to-client traffic (café/guest networks with
client isolation): the leaf won't sit uselessly on it, it roams to one where
the hub actually answers. It also re-scans and rejoins if WiFi drops.

### Flash persistence (survives power loss)

The firmware mounts a **~13 MB FAT partition** (`partitions.csv`, wear-leveled)
at `/data` and keeps mirrored cores there via a blocking `std::fs` storage
backend (`Storage::new_file_storage`, `RandomAccessFile` in the vendored
hypercore). On boot it reopens the control core from flash and re-registers
every announced core — so after a power cycle the leaf reloads its whole
project from flash and only syncs deltas, instead of re-downloading. Verified
on hardware: after a reboot with no hub reachable, the board logs
`reloaded N core(s) from persisted control core` and `persisted core … length=…`
for each core, with all blocks intact.

Two flashing notes:
- `espflash` uses its own default partition table; pass
  `--partition-table partitions.csv` so the FAT `storage` partition exists.
- You can **pre-provision** a leaf without it ever touching the network:
  mirror on a host (`leaf-host --fs-storage DIR`), build a wear-leveled image
  with the IDF's `components/fatfs/wl_fatfsgen.py … --sector_size 4096
  --long_name_support`, and `espflash write-bin 0x310000 image.img`. The board
  reloads those cores on boot. (This is also how persistence was validated
  where the only WiFi available had client isolation.)

### BLE provisioning (set up over Bluetooth, no cfg.toml needed)

A leaf flashed with an **empty / incomplete `cfg.toml`** boots into *provisioning
mode* (blue LED) and advertises a small BLE GATT service. Any listam app then
writes its WiFi credentials + this project's control key + hub address over
Bluetooth; the leaf persists them to NVS and reboots into the normal path above.
A board with a **complete** baked `cfg.toml` never enters provisioning, so
existing boards are unaffected.

- **Headless**: with the bridge running, send the `provision-leaf` op (see the
  listam-headless README). Needs the optional `@abandonware/noble` dependency +
  a BLE radio on the host.
- **Mobile** (Settings → *Pair a leaf*) and **Desktop** (Peers & Devices →
  *Pair over Bluetooth*) read the control key + hub address from the hub they
  already track and write them over react-native-ble-plx / Web Bluetooth.
- The wire contract (service/characteristic UUIDs, payload schema, CRC framing)
  lives in `@listam/provisioning`; the firmware side is `components/leaf_prov`
  (a NimBLE C shim) + `src/config.rs` (NVS-backed runtime config) +
  `src/provisioning.rs`. `cfg.toml` stays as a factory-default fallback.
- **Trust (v1):** the payload is sent in cleartext, trusting physical BLE
  proximity; a version field is reserved for a future PIN/encryption upgrade.
- To re-provision a configured board, clear its NVS record (or press
  **BOOT/GPIO0** just after power-on — best-effort, since GPIO0 is a strapping
  pin); the easiest test path is simply flashing a blank `cfg.toml`.

### ESP-specific gotchas (all handled, noted for the next board)

- **eventfd VFS**: `async-io`'s reactor needs `esp_vfs_eventfd_register()`
  called once at boot, or every connect fails with
  "failed to initialize eventfd for polling". Done in `main`.
- **Memory pages**: `random-access-memory` defaults to **1 MiB pages**,
  allocated whole on first write — a few cores exhaust PSRAM and `abort()`.
  The mirror uses `Storage::new_memory_with_page_size(64 KiB)` instead.
- **`snow` + `ring`**: snow 0.10's `std` feature force-enables `ring`
  (`ring/std`, not `ring?/std`), which has no Xtensa backend → cross-endian
  link error. Vendored `vendor/snow` with the one-character fix.
- **Logging**: leaf-core logs via `tracing`; the ESP captures the `log` crate
  with no tracing subscriber, so key milestones are also emitted via `log`.
  Do **not** enable tracing's `log` feature — it turns every protocol
  instrument span into UART spam and throttles the device.
- **Native USB-CDC console**: reflashing can wedge the serial console (port
  enumerates but `espflash` can't attach). Unplug/replug to recover.
- **Client isolation, empirically**: the leaf logs every scanned network with
  RSSI and prints its IP; if the Mac can ping the board but TCP times out,
  suspect AP client isolation, not the firmware.

## Tests

- `cargo test` (workspace) — includes golden-vector tests generated from the
  installed JS hypercore 11.33 (`bridge-js/gen-vectors.mjs` →
  `testdata/vectors.json`): manifest codec, manifestHash, v1 signables,
  multisig encoding, signature verification.
- `node bridge-js/e2e.mjs` — end-to-end against the real headless app:
  bridge → leaf mirrors → hub killed → a fresh JS corestore peer pulls every
  verified block from the leaf. Prints `E2E RESULT: PASS`.
- `node bridge-js/spike-server.mjs` / `spike-client.mjs` — minimal one-core
  interop harnesses used during bring-up.

## Vendored crates (`vendor/`)

The leaf needed hypercore **v11 manifest** support that the upstream datrs
crates ([hypercore](https://github.com/datrs/hypercore),
[hypercore-protocol-rs](https://github.com/datrs/hypercore-protocol-rs),
hypercore_schema) don't have yet, plus several wire-compat fixes against
hypercore 11.33 / corestore 7.10 / protomux 3.11. All changes are candidates
for upstreaming:

- **hypercore_schema**: v11 `Manifest`/`MultiSignature` codecs,
  `manifest_hash`, `tree_signable_v1`, `verify_manifest_signature`;
  `RequestSeek.padding` (hypercore 11 wire field).
- **hypercore**: open cores by raw key (`HypercoreBuilder::raw_key`) without
  an ed25519 keypair, `set_manifest()` (validate hash == key, persist,
  enable verification), `CoreVerifier` (compat v0 vs manifest v1) threaded
  through proof verification, header/oplog persistence of v1 manifests,
  `Option<PartialKeypair>` for keyless mirrors; `Storage::new_file_storage`
  (blocking `std::fs` backend, works on ESP-IDF FATFS) and
  `new_memory_with_page_size`. **Oplog `Entry` decode bug fixed**: tree_upgrade
  and bitfield were decoded under `flags & 2` (tree_nodes' bit) instead of
  their own bits `4` and `8`, so any entry with tree_nodes + bitfield but no
  upgrade (e.g. a replicated data block) overran the buffer. Invisible with
  in-memory storage (entries are never decoded from bytes), fatal on reopen
  from disk. Regression tests in `oplog/entry.rs`.
- **hypercore-protocol**: `Data.manifest` (flag 16), `Synchronize`
  has_manifest/allow_push flags, `Want`/`Unwant` `any`, `NoData.reason`,
  protomux **batch decode** fixed to spec (len-first, len==0 channel switch,
  control messages in batches, session reject tolerated), and the critical
  protomux **batch encode** fix: `Open`/`Close` now encode as channel-0
  control messages with type tags — previously batched opens were silently
  dropped by JS peers.

## Known limitations / next steps

- **Patched multisig signatures are rejected** ("Manifest signature
  verification failed"): autobase *optimistic* writers (every invited member
  device's first blocks) sign with patch upgrades the verifier doesn't
  reconstruct yet. Until ported (verifier.js `_verifyMulti` patch path),
  member writer cores may not mirror; owner cores are unaffected.
- The leaf **reconnects when it learns new cores** instead of opening
  channels mid-connection (JS-side behavior with late opens was unreliable;
  reconnect guarantees handshake-time opens). Expect a connection bounce
  when project membership changes.
- ESP32 storage now **persists to flash** (FAT partition, see below). It
  still uses RAM (64 KiB pages) if the FAT mount fails. SD-card storage (XIAO
  ESP32-S3 *Sense*) is a future option for projects larger than the ~13 MB FAT
  partition.
- The bridge announces bootstrap/local/system/active-writer cores. Autobase
  views are derived locally by real peers and intentionally not announced.
- Mobile: `lib/leaf-bridge.mjs` is platform-neutral @listam/backend code;
  wiring it into the mobile app config is pending.
