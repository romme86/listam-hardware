//! Multi-core mirror over a single hypercore-protocol connection.
//!
//! The mirror maintains a registry of cores (by key), replicates all of them
//! over one wire connection (download-everything policy, serves requests),
//! and learns new core keys from a designated "control core": a hub-written
//! hypercore carrying JSON lines like `{"add": ["<key hex>", ...]}`.

use std::collections::{HashMap, HashSet};
#[cfg(feature = "disk")]
use std::path::PathBuf;
use std::sync::Arc;

use async_lock::Mutex;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::{AsyncRead, AsyncWrite, channel::mpsc, future::FutureExt};
use hypercore::{Hypercore, HypercoreBuilder, Storage};
use hypercore_protocol::{Channel, Event, Message, Protocol, discovery_key, schema::*};
use hypercore_schema::{RequestBlock, RequestUpgrade};
use tracing::{debug, info, warn};

/// Where mirrored cores keep their data.
#[derive(Debug, Clone)]
pub enum MirrorStorage {
    /// Everything in RAM, lost on restart.
    Memory,
    /// Blocking `std::fs` files, one directory per core (hex of discovery key)
    /// under the given root. Persists across restarts and works on ESP-IDF
    /// (FATFS) as well as the host — no async runtime required.
    StdFs(std::path::PathBuf),
    /// One directory per core, via `random-access-disk` (host/tokio only).
    #[cfg(feature = "disk")]
    Disk(PathBuf),
}

/// A mirrored core plus its replication bookkeeping.
pub struct CoreHandle {
    pub key: [u8; 32],
    pub core: Arc<Mutex<Hypercore>>,
    pub is_control: bool,
}

/// Shared registry of all mirrored cores, keyed by discovery key.
#[derive(Clone)]
pub struct Registry {
    storage: MirrorStorage,
    cores: Arc<Mutex<HashMap<[u8; 32], Arc<CoreHandle>>>>,
}

impl Registry {
    pub fn new(storage: MirrorStorage) -> Self {
        Registry {
            storage,
            cores: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Open (or create) a core for the given key and add it to the registry.
    /// Returns None if the core was already present.
    pub async fn add_core(
        &self,
        key: [u8; 32],
        is_control: bool,
    ) -> anyhow::Result<Option<Arc<CoreHandle>>> {
        let dkey = discovery_key(&key);
        {
            let cores = self.cores.lock().await;
            if cores.contains_key(&dkey) {
                return Ok(None);
            }
        }
        let storage = match &self.storage {
            // 64 KiB pages instead of the 1 MiB default: a leaf mirrors many
            // small cores, and 1 MiB-per-core exhausts PSRAM on the ESP32.
            MirrorStorage::Memory => Storage::new_memory_with_page_size(64 * 1024).await?,
            MirrorStorage::StdFs(root) => {
                let dir = root.join(hex::encode(dkey));
                Storage::new_file_storage(&dir).await?
            }
            #[cfg(feature = "disk")]
            MirrorStorage::Disk(root) => {
                let dir = root.join(hex::encode(dkey));
                Storage::new_disk(&dir, false).await?
            }
        };
        let core = HypercoreBuilder::new(storage).raw_key(key).build().await?;
        let handle = Arc::new(CoreHandle {
            key,
            core: Arc::new(Mutex::new(core)),
            is_control,
        });
        self.cores.lock().await.insert(dkey, handle.clone());
        info!(
            "registered core {} (control: {})",
            hex::encode(key),
            is_control
        );
        Ok(Some(handle))
    }

    /// Re-apply all entries of an already-persisted control core, e.g. after
    /// a restart, so every known core is opened at handshake time.
    pub async fn seed_from_control(&self) -> anyhow::Result<usize> {
        let control = {
            let cores = self.cores.lock().await;
            cores.values().find(|h| h.is_control).cloned()
        };
        let Some(control) = control else {
            return Ok(0);
        };
        let mut keys: Vec<[u8; 32]> = Vec::new();
        {
            let mut core = control.core.lock().await;
            let length = core.info().contiguous_length;
            for index in 0..length {
                if let Some(value) = core.get(index).await? {
                    collect_control_keys(&value, &mut keys);
                }
            }
        }
        let mut added = 0;
        for key in keys {
            if self.add_core(key, false).await?.is_some() {
                added += 1;
            }
        }
        if added > 0 {
            info!("seeded {added} core(s) from persisted control core");
        }
        Ok(added)
    }

    pub async fn get(&self, dkey: &[u8; 32]) -> Option<Arc<CoreHandle>> {
        self.cores.lock().await.get(dkey).cloned()
    }

    pub async fn keys(&self) -> Vec<[u8; 32]> {
        self.cores.lock().await.values().map(|h| h.key).collect()
    }

    /// Total blocks stored across all cores (for status logging).
    pub async fn stats(&self) -> Vec<(String, u64, u64)> {
        // Snapshot the handles first: holding the registry lock while
        // awaiting core locks can deadlock against the connection driver.
        let handles: Vec<Arc<CoreHandle>> = {
            let cores = self.cores.lock().await;
            cores.values().cloned().collect()
        };
        let mut out = Vec::new();
        for handle in handles {
            let info = handle.core.lock().await.info();
            out.push((
                hex::encode(handle.key),
                info.length,
                info.contiguous_length,
            ));
        }
        out
    }
}

/// How often an idle, behind channel nudges the peer again.
const RESYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// How often the connection driver checks for receive silence.
const IDLE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4);

/// Receive silence after which the link is declared dead and redialed. The
/// hub keepalives every ~5s, so this is three missed beats. Needed because
/// the cipher layer swallows EOF/reset on its inbound stream: a peer that
/// vanishes while we are idle would otherwise park the session forever.
const IDLE_DISCONNECT: std::time::Duration = std::time::Duration::from_secs(16);

/// Wraps the raw transport and timestamps every successful read so the
/// connection driver can detect a silently dead link.
struct ActivityIo<IO> {
    io: IO,
    last_read: Arc<std::sync::Mutex<std::time::Instant>>,
}

impl<IO: AsyncRead + Unpin> AsyncRead for ActivityIo<IO> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let result = std::pin::Pin::new(&mut self.io).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &result {
            if *n > 0 {
                *self.last_read.lock().unwrap() = std::time::Instant::now();
            }
        }
        result
    }
}

impl<IO: AsyncWrite + Unpin> AsyncWrite for ActivityIo<IO> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_close(cx)
    }
}

/// Notifications flowing from per-channel tasks back to the connection driver.
enum DriverMsg {
    /// Control core announced new core keys to mirror.
    AddCores(Vec<[u8; 32]>),
}

/// Run the mirror over one established, encrypted, framed protocol stream.
/// `io` is the raw duplex transport (e.g. a TCP stream). Returns when the
/// connection closes or errors.
pub async fn run_connection<IO>(
    io: IO,
    is_initiator: bool,
    registry: Registry,
) -> anyhow::Result<()>
where
    IO: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
{
    use hypercore_handshake::{
        Cipher,
        state_machine::{SecStream, hc_specific::generate_keypair},
    };
    use uint24le_framing::Uint24LELengthPrefixedFraming;

    let last_read = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let io = ActivityIo {
        io,
        last_read: last_read.clone(),
    };
    let framed = Uint24LELengthPrefixedFraming::new(io);
    let cipher = if is_initiator {
        let ss = SecStream::new_initiator_xx(&[])?;
        Cipher::new(Some(Box::new(framed)), ss.into())
    } else {
        let keypair = generate_keypair()?;
        let ss = SecStream::new_responder_xx(&keypair, &[])?;
        Cipher::new(Some(Box::new(framed)), ss.into())
    };
    let mut protocol = Protocol::new(Box::new(cipher));

    let (driver_tx, mut driver_rx) = mpsc::unbounded::<DriverMsg>();
    let mut channel_tasks = FuturesUnordered::new();
    // One open per core per connection. The remote announces cores we may
    // have already opened at handshake time; opening again puts a duplicate
    // Open on the wire, and the JS side pairs only the first — the duplicate
    // channel stays half-dead and its block requests are never answered.
    let mut opened: HashSet<[u8; 32]> = HashSet::new();

    let mut idle_timer = async_io::Timer::after(IDLE_CHECK_INTERVAL);
    loop {
        futures::select! {
            _ = futures::FutureExt::fuse(&mut idle_timer) => {
                idle_timer = async_io::Timer::after(IDLE_CHECK_INTERVAL);
                let idle = last_read.lock().unwrap().elapsed();
                if idle > IDLE_DISCONNECT {
                    log::warn!("link silent for {idle:?}; dropping connection to redial");
                    break;
                }
            }
            event = protocol.next().fuse() => {
                let Some(event) = event else {
                    info!("protocol stream ended");
                    break;
                };
                let event = event?;
                debug!("driver event: {event:?}");
                match event {
                    Event::Handshake(_) => {
                        log::info!("driver: Handshake event (initiator: {is_initiator})");
                        if is_initiator {
                            let keys = registry.keys().await;
                            log::info!("handshake done, opening {} core(s)", keys.len());
                            for key in keys {
                                if !opened.insert(discovery_key(&key)) {
                                    continue;
                                }
                                // Never await opens inline: the command queue
                                // drains only while the protocol is polled.
                                channel_tasks
                                    .push(open_task(protocol.open(key), key).boxed());
                            }
                        }
                    }
                    Event::DiscoveryKey(dkey) => {
                        let handle = registry.get(&dkey).await;
                        log::info!(
                            "driver: remote announced dk={} known={} opened={}",
                            hex::encode(&dkey[..4]),
                            handle.is_some(),
                            opened.contains(&dkey)
                        );
                        if let Some(handle) = handle {
                            if opened.insert(dkey) {
                                channel_tasks.push(
                                    open_task(protocol.open(handle.key), handle.key).boxed(),
                                );
                            }
                        }
                    }
                    Event::Channel(channel) => {
                        let handle = registry.get(channel.discovery_key()).await;
                        log::info!(
                            "driver: channel event dk={} known={}",
                            hex::encode(&channel.discovery_key()[..4]),
                            handle.is_some()
                        );
                        if let Some(handle) = handle {
                            channel_tasks.push(
                                core_channel(channel, handle, registry.clone(), driver_tx.clone())
                                    .boxed(),
                            );
                        }
                    }
                    Event::Close(dkey) => {
                        log::info!("driver: channel closed dk={}", hex::encode(&dkey[..4]));
                        // Allow a clean re-open if the remote closes and
                        // re-announces the core within this connection.
                        opened.remove(&dkey);
                    }
                    _ => {}
                }
            }
            msg = driver_rx.next() => {
                if let Some(DriverMsg::AddCores(keys)) = msg {
                    let mut added = 0;
                    for key in keys {
                        if registry.add_core(key, false).await?.is_some() {
                            added += 1;
                        }
                    }
                    log::info!("driver: AddCores received, {added} new");
                    if added > 0 {
                        // Channels opened mid-connection are not reliably
                        // answered by the JS side; reconnect instead so every
                        // open happens at handshake time.
                        info!("learned {added} new core(s); reconnecting to open them");
                        break;
                    }
                }
            }
            result = channel_tasks.select_next_some() => {
                if let Err(err) = result {
                    log::warn!("channel task ended with error: {err:#}");
                }
            }
        }
    }
    Ok(())
}

/// Wraps a protocol channel-open future so it can run inside the driver's
/// task set (instead of blocking the event loop on the command queue).
async fn open_task(
    open: impl std::future::Future<Output = std::io::Result<()>>,
    key: [u8; 32],
) -> anyhow::Result<()> {
    debug!("open_task {} awaiting", hex::encode(&key[..4]));
    let result = open
        .await
        .map_err(|err| anyhow::anyhow!("open {}: {err}", hex::encode(key)));
    log::info!("open_task {} done: ok={}", hex::encode(&key[..4]), result.is_ok());
    result
}

/// Per-peer replication state for one core, mirroring the JS replicator's
/// essentials (full-download policy, no sparse mode).
struct PeerState {
    can_upgrade: bool,
    remote_fork: u64,
    remote_length: u64,
    remote_synced: bool,
    length_acked: u64,
    /// Request id counter; ids correlate Data responses to Requests.
    next_request_id: u64,
    /// Set when we have asked for the manifest and not yet received it.
    manifest_requested: bool,
    /// Set while an upgrade request is outstanding.
    inflight_upgrade: bool,
    /// Block index with an outstanding request. Keeps the download to a
    /// single chain: without this, a resync tick firing mid-download starts
    /// a second chain and every remaining block gets fetched and applied
    /// twice (doubling flash writes).
    inflight_block: Option<u64>,
}

impl Default for PeerState {
    fn default() -> Self {
        PeerState {
            can_upgrade: true,
            remote_fork: 0,
            remote_length: 0,
            remote_synced: false,
            length_acked: 0,
            next_request_id: 0,
            manifest_requested: false,
            inflight_upgrade: false,
            inflight_block: None,
        }
    }
}

impl PeerState {
    fn next_id(&mut self) -> u64 {
        self.next_request_id += 1;
        self.next_request_id
    }
}

async fn core_channel(
    mut channel: Channel,
    handle: Arc<CoreHandle>,
    registry: Registry,
    driver_tx: mpsc::UnboundedSender<DriverMsg>,
) -> anyhow::Result<()> {
    let label = short_label(&handle.key);
    let mut state = PeerState::default();

    // Announce our state.
    let (info, has_manifest) = {
        let core = handle.core.lock().await;
        (core.info(), core.manifest().is_some())
    };
    log::info!(
        "[{label}] channel task start: len={} contig={} manifest={has_manifest}",
        info.length,
        info.contiguous_length
    );
    let mut first_messages = vec![Message::Synchronize(Synchronize {
        fork: info.fork,
        length: info.length,
        remote_length: 0,
        can_upgrade: state.can_upgrade,
        uploading: true,
        downloading: true,
        has_manifest,
        allow_push: false,
    })];
    if info.contiguous_length > 0 {
        first_messages.push(Message::Range(Range {
            drop: false,
            start: 0,
            length: info.contiguous_length,
        }));
    }
    if !has_manifest {
        state.manifest_requested = true;
        first_messages.push(Message::Request(Request {
            id: state.next_id(),
            fork: info.fork,
            block: None,
            hash: None,
            seek: None,
            upgrade: None,
            manifest: true,
            priority: 0,
        }));
    }
    channel.send_batch(&first_messages).await?;

    let mut resync = async_io::Timer::after(RESYNC_INTERVAL);
    loop {
        let message = futures::select! {
            message = channel.next().fuse() => {
                let Some(message) = message else { break };
                Some(message)
            }
            _ = futures::FutureExt::fuse(&mut resync) => {
                resync = async_io::Timer::after(RESYNC_INTERVAL);
                None
            }
        };
        match message {
            Some(message) => {
                match on_message(&mut channel, &handle, &registry, &driver_tx, &mut state, message)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(err) => {
                        // Log and keep the channel alive; the periodic resync
                        // below re-requests anything that got lost.
                        log::warn!("[{label}] error handling message, will resync: {err:#}");
                        state.inflight_upgrade = false;
                        state.inflight_block = None;
                    }
                }
            }
            None => {
                // Periodic resync: if we are behind or have holes, nudge the
                // peer again. Duplicated requests are benign.
                let mut messages: Vec<Message> = vec![];
                {
                    let mut core = handle.core.lock().await;
                    let info = core.info();
                    let has_manifest = core.manifest().is_some();
                    if has_manifest && state.remote_synced {
                        if state.remote_length > info.length && !state.inflight_upgrade {
                            log::info!(
                                "[{label}] resync: behind ({} < {}), re-requesting upgrade",
                                info.length, state.remote_length
                            );
                            messages.push(request_upgrade(&mut state, &info));
                        } else if info.contiguous_length < info.length
                            && state.inflight_block.is_none()
                        {
                            // Heal the first hole in our contiguous range —
                            // but never start a second chain next to an
                            // active block download.
                            let index = info.contiguous_length;
                            let nodes = core
                                .missing_nodes(index)
                                .await
                                .map_err(|e| anyhow::anyhow!("resync missing_nodes({index}): {e}"))?;
                            log::info!("[{label}] resync: healing hole at block {index}");
                            state.inflight_block = Some(index);
                            messages.push(Message::Request(Request {
                                id: state.next_id(),
                                fork: info.fork,
                                block: Some(RequestBlock { index, nodes }),
                                hash: None,
                                seek: None,
                                upgrade: None,
                                manifest: false,
                                priority: 0,
                            }));
                        }
                    }
                }
                if !messages.is_empty() {
                    channel.send_batch(&messages).await?;
                }
            }
        }
    }
    debug!("[{label}] channel loop ended");
    Ok(())
}

async fn on_message(
    channel: &mut Channel,
    handle: &Arc<CoreHandle>,
    registry: &Registry,
    driver_tx: &mpsc::UnboundedSender<DriverMsg>,
    state: &mut PeerState,
    message: Message,
) -> anyhow::Result<bool> {
    let label = short_label(&handle.key);
    match message {
        Message::Synchronize(message) => {
            debug!("[{label}] Synchronize {message:?}");
            let length_changed = message.length != state.remote_length;
            let first_sync = !state.remote_synced;
            let (info, has_manifest) = {
                let core = handle.core.lock().await;
                (core.info(), core.manifest().is_some())
            };
            let same_fork = message.fork == info.fork;

            state.remote_fork = message.fork;
            state.remote_length = message.length;
            state.remote_synced = true;
            state.length_acked = if same_fork { message.remote_length } else { 0 };

            let mut messages = vec![];
            if first_sync {
                messages.push(Message::Synchronize(Synchronize {
                    fork: info.fork,
                    length: info.length,
                    remote_length: state.remote_length,
                    can_upgrade: state.can_upgrade,
                    uploading: true,
                    downloading: true,
                    has_manifest,
                    allow_push: false,
                }));
            }
            if has_manifest
                && state.remote_length > info.length
                && !state.inflight_upgrade
                && (length_changed || first_sync)
            {
                messages.push(request_upgrade(state, &info));
            }
            channel.send_batch(&messages).await?;
        }
        Message::Request(message) => {
            debug!("[{label}] Request {message:?}");
            let (proof, manifest, fork) = {
                let mut core = handle.core.lock().await;
                let proof = core
                    .create_proof(message.block, message.hash, message.seek, message.upgrade)
                    .await?;
                let manifest = if message.manifest {
                    core.manifest().cloned()
                } else {
                    None
                };
                (proof, manifest, core.info().fork)
            };
            if proof.is_some() || manifest.is_some() {
                let proof = proof.unwrap_or(hypercore_schema::Proof {
                    fork,
                    block: None,
                    hash: None,
                    seek: None,
                    upgrade: None,
                });
                channel
                    .send(Message::Data(Data {
                        request: message.id,
                        fork: proof.fork,
                        hash: proof.hash,
                        block: proof.block,
                        seek: proof.seek,
                        upgrade: proof.upgrade,
                        manifest,
                    }))
                    .await?;
            } else {
                channel
                    .send(Message::NoData(NoData {
                        request: message.id,
                        reason: 0,
                    }))
                    .await?;
            }
        }
        Message::Data(message) => {
            debug!(
                "[{label}] Data req={} fork={} block={:?} upgrade={:?} manifest={}",
                message.request,
                message.fork,
                message.block.as_ref().map(|b| (b.index, b.nodes.len())),
                message.upgrade.as_ref().map(|u| (u.start, u.length, u.nodes.len())),
                message.manifest.is_some()
            );
            let mut messages: Vec<Message> = vec![];
            let mut new_blocks: Vec<(u64, Vec<u8>)> = vec![];
            {
                let mut core = handle.core.lock().await;

                if message.upgrade.is_some() {
                    state.inflight_upgrade = false;
                }
                if message.block.is_some() {
                    // The outstanding block request was answered; the chain
                    // re-arms below if more blocks are missing.
                    state.inflight_block = None;
                }
                if let Some(manifest) = &message.manifest {
                    log::info!("[{label}] received manifest, enabling verification");
                    core.set_manifest(manifest.clone())
                        .await
                        .map_err(|e| anyhow::anyhow!("set_manifest: {e}"))?;
                    state.manifest_requested = false;
                    let info = core.info();
                    // Now that we can verify, ask for the upgrade if behind.
                    if state.remote_length > info.length && !state.inflight_upgrade {
                        messages.push(request_upgrade(state, &info));
                    }
                }

                let has_content = message.block.is_some()
                    || message.hash.is_some()
                    || message.seek.is_some()
                    || message.upgrade.is_some();
                if has_content {
                    let old_info = core.info();
                    let block_index = message.block.as_ref().map(|b| b.index);
                    let upgrade_meta = message.upgrade.as_ref().map(|u| (u.start, u.length));
                    let proof = message.clone().into_proof();
                    let applied = core
                        .verify_and_apply_proof(&proof)
                        .await
                        .map_err(|e| anyhow::anyhow!("verify_and_apply_proof: {e}"))?;
                    let new_info = core.info();
                    log::info!(
                        "[{label}] proof applied={applied} block={block_index:?} upgrade={upgrade_meta:?} len {}→{}",
                        old_info.length,
                        new_info.length
                    );

                    if applied {
                        if let Some(index) = block_index {
                            if let Some(value) = core
                                .get(index)
                                .await
                                .map_err(|e| anyhow::anyhow!("get({index}): {e}"))?
                            {
                                new_blocks.push((index, value));
                            }
                            let next = index + 1;
                            if next < new_info.length {
                                // Ask for the next missing block in the
                                // verified region.
                                let nodes = core
                                    .missing_nodes(next)
                                    .await
                                    .map_err(|e| anyhow::anyhow!("missing_nodes({next}): {e}"))?;
                                state.inflight_block = Some(next);
                                messages.push(Message::Request(Request {
                                    id: state.next_id(),
                                    fork: new_info.fork,
                                    block: Some(RequestBlock { index: next, nodes }),
                                    hash: None,
                                    seek: None,
                                    upgrade: None,
                                    manifest: false,
                                    priority: 0,
                                }));
                            } else if state.remote_length > new_info.length {
                                // Remote grew while we were catching up:
                                // extend the verified region first.
                                messages.push(request_upgrade(state, &new_info));
                            }
                        }
                        if let Some((_start, length)) = upgrade_meta {
                            let new_length = old_info.length.max(_start + length);
                            messages.push(Message::Synchronize(Synchronize {
                                fork: new_info.fork,
                                length: new_length,
                                remote_length: if new_info.fork == state.remote_fork {
                                    state.remote_length
                                } else {
                                    0
                                },
                                can_upgrade: false,
                                uploading: true,
                                downloading: true,
                                has_manifest: core.manifest().is_some(),
                                allow_push: false,
                            }));
                            // Start fetching the first missing block of the new range.
                            if old_info.length < state.remote_length && state.inflight_block.is_none() {
                                let request_index = old_info.length;
                                let nodes = core
                                    .missing_nodes(request_index)
                                    .await
                                    .map_err(|e| anyhow::anyhow!("missing_nodes(upg {request_index}): {e}"))?;
                                state.inflight_block = Some(request_index);
                                messages.push(Message::Request(Request {
                                    id: state.next_id(),
                                    fork: new_info.fork,
                                    block: Some(RequestBlock {
                                        index: request_index,
                                        nodes,
                                    }),
                                    hash: None,
                                    seek: None,
                                    upgrade: None,
                                    manifest: false,
                                    priority: 0,
                                }));
                            }
                        }
                    }
                }
            }
            if !messages.is_empty() {
                for m in &messages {
                    debug!("[{label}] sending {}", message_summary(m));
                }
                channel.send_batch(&messages).await?;
            }
            for (index, value) in new_blocks {
                info!(
                    "[{label}] block {index} stored ({} bytes)",
                    value.len()
                );
                log::info!("[{label}] block {index} stored ({} bytes)", value.len());
                if handle.is_control {
                    apply_control_entry(&value, driver_tx);
                }
                let _ = registry; // reserved for future per-block hooks
            }
        }
        Message::NoData(message) => {
            log::info!("[{label}] NoData for request {}", message.request);
            state.inflight_upgrade = false;
            state.inflight_block = None;
        }
        Message::Want(message) => {
            // Full mirror: we hold everything contiguous; answer with our range.
            debug!("[{label}] Want {message:?}");
            let info = {
                let core = handle.core.lock().await;
                core.info()
            };
            if info.contiguous_length > 0 {
                channel
                    .send(Message::Range(Range {
                        drop: false,
                        start: 0,
                        length: info.contiguous_length,
                    }))
                    .await?;
            }
        }
        Message::Range(message) => {
            debug!("[{label}] Range {message:?}");
            // Remote advertises blocks; if it implies more data than we know
            // from Synchronize, the next Synchronize will trigger requests.
        }
        Message::Extension(message) => {
            debug!("[{label}] Extension {message:?} (ignored)");
        }
        message => {
            debug!("[{label}] unhandled message {message:?}");
        }
    }
    Ok(true)
}

fn request_upgrade(state: &mut PeerState, info: &hypercore::Info) -> Message {
    state.inflight_upgrade = true;
    Message::Request(Request {
        id: state.next_id(),
        fork: info.fork,
        block: None,
        hash: None,
        seek: None,
        upgrade: Some(RequestUpgrade {
            start: info.length,
            length: state.remote_length - info.length,
        }),
        manifest: false,
        priority: 0,
    })
}

/// Parse a control-core entry into keys.
/// Entries are JSON: `{"add": ["<64 hex chars>", ...]}`.
fn collect_control_keys(value: &[u8], keys: &mut Vec<[u8; 32]>) {
    let Ok(text) = std::str::from_utf8(value) else {
        warn!("control entry is not utf-8, ignoring");
        return;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        warn!("control entry is not valid JSON, ignoring: {text}");
        return;
    };
    let Some(add) = parsed.get("add").and_then(|v| v.as_array()) else {
        return;
    };
    for entry in add {
        let Some(hex_key) = entry.as_str() else {
            continue;
        };
        match hex::decode(hex_key) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            _ => warn!("control entry contains invalid key {hex_key}, ignoring"),
        }
    }
}

/// Parse a control-core entry and forward new keys to the driver.
fn apply_control_entry(value: &[u8], driver_tx: &mpsc::UnboundedSender<DriverMsg>) {
    let mut keys = Vec::new();
    collect_control_keys(value, &mut keys);
    if !keys.is_empty() {
        info!("control core: learning {} new core(s)", keys.len());
        let _ = driver_tx.unbounded_send(DriverMsg::AddCores(keys));
    }
}

fn message_summary(m: &Message) -> String {
    match m {
        Message::Request(r) => format!(
            "Request id={} fork={} block={:?} upgrade={:?} manifest={}",
            r.id,
            r.fork,
            r.block.as_ref().map(|b| (b.index, b.nodes)),
            r.upgrade.as_ref().map(|u| (u.start, u.length)),
            r.manifest
        ),
        Message::Synchronize(s) => format!("Synchronize len={} rlen={} fork={}", s.length, s.remote_length, s.fork),
        other => format!("{other:?}"),
    }
}

fn short_label(key: &[u8; 32]) -> String {
    hex::encode(&key[..4])
}
