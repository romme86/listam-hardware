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
use leaf_core::{MirrorStorage, Registry, run_connection};
use log::{error, info, warn};

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
}

/// All configured (ssid, psk) pairs, in priority order.
fn known_networks(config: &Config) -> Vec<(&'static str, &'static str)> {
    [
        (config.wifi_ssid, config.wifi_psk),
        (config.wifi_ssid2, config.wifi_psk2),
        (config.wifi_ssid3, config.wifi_psk3),
    ]
    .into_iter()
    .filter(|(ssid, _)| !ssid.is_empty())
    .collect()
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

    let config = CONFIG;
    let networks = known_networks(&config);
    if networks.is_empty() || config.hub_addr.is_empty() || config.control_key.is_empty() {
        error!("cfg.toml is missing wifi networks / hub_addr / control_key — see cfg.toml.example");
        return Err(anyhow!("missing configuration"));
    }
    let control_key: [u8; 32] = hex::decode(config.control_key)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .context("control_key must be 64 hex chars")?;

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

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
        std::thread::Builder::new()
            .name("leaf".into())
            .stack_size(96 * 1024)
            .spawn(move || {
                if let Err(err) =
                    leaf_main(control_key, config.hub_addr, hub_reachable, storage_root)
                {
                    error!("leaf thread exited: {err:#}");
                }
            })?;
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
