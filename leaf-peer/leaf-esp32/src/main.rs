//! listam leaf peer firmware for the ESP32-S3.
//!
//! Joins the best available of several known WiFi networks (scan → strongest
//! known SSID → connect, falling back through the list), dials the listam
//! leaf bridge(s) over TCP, and mirrors every announced hypercore in RAM
//! (8MB PSRAM), serving them back to any peer. If WiFi drops, a supervisor
//! re-scans and reconnects. Configuration is baked in from `cfg.toml` at
//! build time.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, anyhow};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::prelude::Peripherals,
    log::EspLogger,
    nvs::EspDefaultNvsPartition,
    wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi},
};
use futures::FutureExt;
use esp_idf_svc::hal::gpio::{AnyOutputPin, OutputPin};
use leaf_core::{MirrorStorage, Registry, run_connection};
use log::{error, info, warn};

mod config;
mod led;
mod provisioning;
mod voice;

#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
    #[default("")]
    wifi_ssid2: &'static str,
    #[default("")]
    wifi_psk2: &'static str,
    #[default("")]
    wifi_ssid3: &'static str,
    #[default("")]
    wifi_psk3: &'static str,
    /// Comma-separated list of leaf bridges to dial, e.g.
    /// "192.168.1.10:9993,192.168.4.23:9993" (one per network is typical).
    #[default("")]
    hub_addr: &'static str,
    #[default("")]
    control_key: &'static str,
    /// Side-band TCP host:port the voice thread streams utterances to (the
    /// headless audio bridge). Empty disables the voice front-end.
    #[default("")]
    audio_addr: &'static str,
    /// Loudness gate: a window must peak at/above this dBFS to wake. Negative;
    /// -25 ~ a deliberate close "yo".
    #[default(-25)]
    wake_db_threshold: i32,
    /// End the utterance after this many ms below the wake floor.
    #[default(800)]
    silence_timeout_ms: i32,
    /// Onboard addressable RGB LED pin: 48 (DevKitC-1 v1.0) or 38 (v1.1).
    #[default(48)]
    led_gpio: i32,
}

/// BLE name the leaf advertises in provisioning mode: a stable per-device
/// suffix from the efuse MAC, e.g. "listam-leaf-3F7A".
fn device_name() -> String {
    let mut mac = [0u8; 6];
    unsafe {
        esp_idf_svc::sys::esp_efuse_mac_get_default(mac.as_mut_ptr());
    }
    format!("listam-leaf-{:02X}{:02X}", mac[4], mac[5])
}

/// True if the BOOT button (GPIO0) is held low at power-on — used to force
/// re-provisioning even on an already-configured board. GPIO0 is a strapping
/// pin, so this is read once after boot. Any error reads as "not held".
fn boot_button_held(pin: esp_idf_svc::hal::gpio::Gpio0) -> bool {
    esp_idf_svc::hal::gpio::PinDriver::input(pin)
        .map(|driver| driver.is_low())
        .unwrap_or(false)
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    // async-io's reactor polls via eventfd; ESP-IDF requires the eventfd VFS
    // to be registered before first use. One per reactor is enough, but give
    // headroom for the per-connection async sockets.
    esp_idf_svc::sys::esp!(unsafe {
        esp_idf_svc::sys::esp_vfs_eventfd_register(
            &esp_idf_svc::sys::esp_vfs_eventfd_config_t { max_fds: 5 },
        )
    })
    .context("registering eventfd VFS for async-io")?;

    // Mount the FAT data partition under /data so mirrored cores persist
    // across power cycles (re-formatted on first boot). Returns the dir the
    // leaf keeps cores in.
    let storage_root = match mount_fat_storage() {
        Ok(root) => {
            info!("persistent storage mounted at {root}");
            Some(root)
        }
        Err(err) => {
            warn!("could not mount FAT storage ({err:#}); falling back to RAM");
            None
        }
    };
    // Replay the exact ops hypercore's file storage performs (64-char hex
    // dir, open-create, seek+write, metadata) so a broken FS is diagnosed
    // per-op at boot and degrades to RAM mirroring instead of killing the
    // leaf thread.
    let storage_root = storage_root.and_then(|root| match storage_self_check(&root) {
        Ok(()) => Some(root),
        Err(err) => {
            warn!("storage self-check failed ({err:#}); falling back to RAM mirroring");
            None
        }
    });

    let baked = CONFIG;

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Resolve the runtime configuration. Precedence: an NVS record written by
    // BLE provisioning > a complete baked cfg.toml (backward compatibility) >
    // none → enter BLE provisioning mode. Holding BOOT (GPIO0) at power-on
    // forces re-provisioning even on an already-configured board.
    let provisioned = config::load(&nvs).unwrap_or_else(|err| {
        warn!("reading provisioning record failed ({err:#}); ignoring");
        None
    });
    let force_reprovision = boot_button_held(peripherals.pins.gpio0);
    let runtime = if force_reprovision {
        None
    } else {
        provisioned.or_else(|| config::from_toml(&baked))
    };

    // The LED pin/channel is shared between provisioning status and the voice
    // front-end; pick the pin from the effective config (runtime overrides baked).
    let led_gpio = runtime.as_ref().map(|rc| rc.led_gpio).unwrap_or(baked.led_gpio);
    // Claim the voice peripherals before WiFi consumes the modem (disjoint
    // fields). i2s0 + GPIO4/5/6 (mic) + RMT ch0 + the LED pin are otherwise free.
    let voice_i2s = peripherals.i2s0;
    let voice_bclk = peripherals.pins.gpio4;
    let voice_din = peripherals.pins.gpio6;
    let voice_ws = peripherals.pins.gpio5;
    let voice_rmt = peripherals.rmt.channel0;
    let led_pin: AnyOutputPin = if led_gpio == 38 {
        peripherals.pins.gpio38.downgrade_output()
    } else {
        peripherals.pins.gpio48.downgrade_output()
    };

    // Unprovisioned (or a forced re-pair): advertise over BLE and wait for a
    // listam app to write WiFi creds + the control key. We start neither WiFi
    // nor the leaf/voice threads here, leaving internal RAM free for the BLE
    // stack; on success the config is stored to NVS and the leaf reboots into
    // the normal path below.
    let Some(runtime) = runtime else {
        if force_reprovision {
            info!("BOOT held at power-on — forcing re-provisioning");
        } else {
            info!("no configuration found — entering BLE provisioning mode");
        }
        let mut led = led::Led::new(voice_rmt, led_pin)?;
        let name = device_name();
        if let Err(err) = provisioning::run(&nvs, &mut led, &name) {
            error!("provisioning failed to start ({err:#}); rebooting");
        }
        // provisioning::run loops until success then esp_restart()s; reaching
        // here is only a startup-error path — restart to retry cleanly.
        unsafe { esp_idf_svc::sys::esp_restart() };
        unreachable!("esp_restart does not return");
    };

    // --- Normal (provisioned) path -------------------------------------------
    // toml_cfg yields &'static str; the runtime values are owned. The config
    // lives for the whole program, so leak the owned strings to &'static str
    // and keep every downstream signature unchanged.
    let control_key = runtime.control_key;
    let hub_addr: &'static str = Box::leak(runtime.hub_addr.into_boxed_str());
    let audio_addr: &'static str = Box::leak(runtime.audio_addr.into_boxed_str());
    let wake_db_threshold = runtime.wake_db_threshold;
    let silence_timeout_ms = runtime.silence_timeout_ms;
    let networks: Vec<(&'static str, &'static str)> = runtime
        .networks
        .into_iter()
        .map(|(ssid, psk)| {
            (
                &*Box::leak(ssid.into_boxed_str()),
                &*Box::leak(psk.into_boxed_str()),
            )
        })
        .collect();

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;
    wifi.start()?;

    // Shared signal: the leaf thread sets this true whenever it reaches a hub.
    // The supervisor uses it to roam off networks where no hub is reachable
    // (e.g. a strong café AP with client isolation).
    let hub_reachable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    {
        let hub_reachable = hub_reachable.clone();
        // Non-fatal: if the 96KB mirror thread can't get a stack (tight internal
        // RAM, e.g. with the wake-word model linked), the voice/wake front-end
        // (16KB) should still run rather than taking the whole leaf down.
        let spawned = std::thread::Builder::new()
            .name("leaf".into())
            .stack_size(96 * 1024)
            .spawn(move || {
                if let Err(err) = leaf_main(control_key, hub_addr, hub_reachable, storage_root) {
                    error!("leaf thread exited: {err:#}");
                }
            });
        if let Err(e) = spawned {
            error!("leaf mirror thread failed to spawn ({e}); voice front-end still runs");
        }
    }

    // Voice front-end (optional): capture the mic, gate on loudness, and stream
    // utterances to the headless audio bridge, with RGB-LED feedback.
    if !audio_addr.is_empty() {
        let wake_thr = wake_db_threshold as f32;
        let silence_ms = silence_timeout_ms.max(100) as u32;
        let free_internal = unsafe {
            esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL)
        };
        info!("free internal RAM before voice thread: {free_internal} bytes");
        std::thread::Builder::new()
            .name("voice".into())
            .stack_size(16 * 1024)
            .spawn(move || {
                voice::run(
                    voice_i2s, voice_bclk, voice_din, voice_ws, voice_rmt, led_pin, audio_addr,
                    wake_thr, silence_ms,
                )
            })?;
        info!("voice front-end enabled (streaming to {audio_addr})");
    }

    // WiFi supervisor: a leaf's goal is reaching a hub, not signal bars. Pick
    // a known network, and if no hub becomes reachable through it within the
    // grace period, rotate to the next known network. Also re-scans if WiFi
    // drops entirely.
    let mut rotate = 0usize;
    loop {
        connect_known_network(&mut wifi, &networks, rotate)?;
        hub_reachable.store(false, std::sync::atomic::Ordering::SeqCst);

        // Give the leaf thread time to dial every hub over this network.
        let grace = Duration::from_secs(25);
        let start = std::time::Instant::now();
        let mut had_hub = false;
        while start.elapsed() < grace {
            std::thread::sleep(Duration::from_secs(2));
            if hub_reachable.load(std::sync::atomic::Ordering::SeqCst) {
                had_hub = true;
                break;
            }
            if !wifi.is_connected().unwrap_or(false) {
                break;
            }
        }

        if had_hub {
            info!("hub reachable on current network; holding");
            // Stay until WiFi drops; periodically confirm a hub is still there.
            loop {
                std::thread::sleep(Duration::from_secs(10));
                if !wifi.is_connected().unwrap_or(false) {
                    warn!("wifi lost, re-scanning ...");
                    break;
                }
            }
        } else {
            warn!("no hub reachable on current network, rotating to the next known one");
            let _ = wifi.disconnect();
            rotate = rotate.wrapping_add(1);
        }
    }
}

/// Scan and connect to a known network. Among visible known networks (sorted
/// by signal), `rotate` selects which one to try first, so repeated calls
/// cycle through them — letting the supervisor roam off an isolated network.
/// If none are visible, blindly tries each configured network.
fn connect_known_network(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    networks: &[(&'static str, &'static str)],
    rotate: usize,
) -> anyhow::Result<()> {
    loop {
        // Visible known networks, strongest signal first.
        let mut candidates: Vec<(usize, i8)> = match wifi.scan() {
            Ok(access_points) => {
                let visible: Vec<String> = access_points
                    .iter()
                    .map(|ap| format!("{} ({}dBm)", ap.ssid, ap.signal_strength))
                    .collect();
                info!("scan found {} network(s): {}", visible.len(), visible.join(", "));
                networks
                    .iter()
                    .enumerate()
                    .filter_map(|(index, (ssid, _))| {
                        access_points
                            .iter()
                            .filter(|ap| ap.ssid == *ssid)
                            .map(|ap| ap.signal_strength)
                            .max()
                            .map(|rssi| (index, rssi))
                    })
                    .collect()
            }
            Err(err) => {
                warn!("wifi scan failed ({err}), will try all known networks");
                vec![]
            }
        };
        candidates.sort_by_key(|(_, rssi)| std::cmp::Reverse(*rssi));

        let mut order: Vec<usize> = if candidates.is_empty() {
            warn!("no known network visible, trying all configured ones");
            (0..networks.len()).collect()
        } else {
            candidates.iter().map(|(index, _)| *index).collect()
        };
        // Rotate the preference so the supervisor can move past a network
        // whose hub is unreachable instead of always re-picking the strongest.
        if !order.is_empty() {
            let shift = rotate % order.len();
            order.rotate_left(shift);
        }

        for index in order {
            let (ssid, psk) = networks[index];
            info!("connecting to wifi '{ssid}' ...");
            let client_config = ClientConfiguration {
                ssid: ssid.try_into().map_err(|_| anyhow!("ssid too long"))?,
                password: psk.try_into().map_err(|_| anyhow!("password too long"))?,
                auth_method: if psk.is_empty() {
                    AuthMethod::None
                } else {
                    AuthMethod::WPA2Personal
                },
                ..Default::default()
            };
            if let Err(err) = wifi.set_configuration(&Configuration::Client(client_config)) {
                warn!("'{ssid}': set_configuration failed: {err}");
                continue;
            }
            match wifi.connect().and_then(|()| wifi.wait_netif_up()) {
                Ok(()) => {
                    let ip = wifi.wifi().sta_netif().get_ip_info()?;
                    info!("wifi up on '{ssid}': {:?}", ip);
                    return Ok(());
                }
                Err(err) => {
                    warn!("'{ssid}': connect failed: {err}");
                    // Make sure the driver is back in a connectable state.
                    let _ = wifi.disconnect();
                }
            }
        }
        warn!("no known network reachable, retrying in 10s");
        std::thread::sleep(Duration::from_secs(10));
    }
}

/// Mount the `storage` FAT partition at `/data` (formatting on first boot)
/// and return the directory cores are kept in. The wear-leveling handle is
/// intentionally leaked: the mount lasts the whole program.
fn mount_fat_storage() -> anyhow::Result<String> {
    use esp_idf_svc::sys::{
        esp, esp_vfs_fat_mount_config_t, esp_vfs_fat_spiflash_mount_rw_wl, wl_handle_t,
        WL_INVALID_HANDLE,
    };
    let base_path = c"/data";
    let partition_label = c"storage";
    let config = esp_vfs_fat_mount_config_t {
        format_if_mount_failed: true,
        max_files: 64, // 4 files per core, so ~16 cores
        allocation_unit_size: 4096,
        disk_status_check_enable: false,
    };
    let mut wl_handle: wl_handle_t = WL_INVALID_HANDLE;
    esp!(unsafe {
        esp_vfs_fat_spiflash_mount_rw_wl(
            base_path.as_ptr(),
            partition_label.as_ptr(),
            &config,
            &mut wl_handle,
        )
    })
    .context("mounting FAT 'storage' partition")?;
    // A freshly formatted partition (first boot, or after an erase-flash) has
    // no directories yet; leaf-core assumes the storage root exists, and the
    // FAT VFS surfaces the missing parent as EPERM, killing the leaf thread.
    std::fs::create_dir_all("/data/cores").context("creating cores dir on FAT storage")?;
    Ok("/data/cores".to_string())
}

fn storage_self_check(root: &str) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let deep = std::path::Path::new(root).join("f".repeat(64));
    std::fs::create_dir_all(&deep).context("create_dir_all 64-char core dir")?;
    let file_path = deep.join("oplog");
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file_path)
            .context("open r/w+create")?;
        file.seek(SeekFrom::Start(0)).context("seek")?;
        file.write_all(b"selfcheck").context("write")?;
        file.sync_all().context("sync_all (fsync)")?;
        // NB: no raw set_len here — this FATFS VFS has no fd-ftruncate
        // (EPERM); the vendored hypercore storage emulates it (grow = zero
        // write at new end, shrink = truncating reopen), so plain set_len
        // failing is expected and fine.
    }
    let len = std::fs::metadata(&file_path).context("metadata")?.len();
    anyhow::ensure!(len >= 9, "file shorter than written ({len})");
    std::fs::remove_file(&file_path).context("remove file")?;
    std::fs::remove_dir(&deep).context("remove dir")?;
    Ok(())
}

fn leaf_main(
    control_key: [u8; 32],
    hub_addrs: &'static str,
    hub_reachable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    storage_root: Option<String>,
) -> anyhow::Result<()> {
    futures::executor::block_on(async move {
        // Live on-device FAT writes need explicit gap zero-filling: FATFS
        // leaves seek-past-EOF regions undefined (stale flash), which used to
        // corrupt the tree store (`Invalid checksum at node 0, store tree`).
        // The StdFs backend (vendor/hypercore/src/storage/file.rs) now
        // zero-fills those gaps on write and set_len-grow, so cores persist
        // correctly. Falls back to RAM only if the FAT mount/self-check failed.
        let storage = match &storage_root {
            Some(root) => MirrorStorage::StdFs(std::path::PathBuf::from(root)),
            None => MirrorStorage::Memory,
        };
        let registry = Registry::new(storage);
        registry.add_core(control_key, true).await?;
        // Re-learn cores announced in a previously synced control core (after a
        // reboot, the control core reloads from flash with its entries).
        let seeded = registry.seed_from_control().await?;
        if seeded > 0 {
            info!("reloaded {seeded} core(s) from persisted control core");
        }
        // Report what we hold right after boot — proves on-device that block
        // data (not just core registration) survived the power cycle.
        for (key, length, contiguous) in registry.stats().await {
            info!(
                "persisted core {} length={length} contiguous={contiguous}",
                &key[..8]
            );
        }

        let loops: Vec<_> = hub_addrs
            .split(',')
            .map(str::trim)
            .filter(|addr| !addr.is_empty())
            .map(|addr| connection_loop(addr, registry.clone(), hub_reachable.clone()).boxed_local())
            .collect();
        if loops.is_empty() {
            return Err(anyhow!("hub_addr is empty"));
        }
        futures::future::join_all(loops).await;
        Ok(())
    })
}

/// Dial one bridge forever, with an async connect (a hub on the other,
/// currently unreachable network must not stall the other loops). Flags
/// `hub_reachable` on every successful connect so the WiFi supervisor knows
/// this network can reach a hub.
async fn connection_loop(
    addr: &'static str,
    registry: Registry,
    hub_reachable: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        info!("connecting to hub {addr} ...");
        match async_connect(addr, Duration::from_secs(8)).await {
            Ok(stream) => {
                hub_reachable.store(true, std::sync::atomic::Ordering::SeqCst);
                info!("connected to {addr}");
                match run_connection(stream, true, registry.clone()).await {
                    Ok(()) => info!("connection to {addr} closed"),
                    Err(err) => error!("connection to {addr} error: {err:#}"),
                }
                for (key, length, contiguous) in registry.stats().await {
                    info!("status core={} length={length} contiguous={contiguous}", &key[..8]);
                }
            }
            Err(err) => error!("could not connect to {addr}: {err}"),
        }
        async_io::Timer::after(Duration::from_secs(3)).await;
    }
}

async fn async_connect(
    addr: &str,
    timeout: Duration,
) -> anyhow::Result<async_io::Async<TcpStream>> {
    let socket_addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("could not resolve {addr}"))?;
    let connect = async_io::Async::<TcpStream>::connect(socket_addr);
    futures::select! {
        result = connect.fuse() => {
            let stream = result?;
            stream.get_ref().set_nodelay(true).ok();
            Ok(stream)
        }
        _ = futures::FutureExt::fuse(async_io::Timer::after(timeout)) => {
            Err(anyhow!("connect timed out after {timeout:?}"))
        }
    }
}
