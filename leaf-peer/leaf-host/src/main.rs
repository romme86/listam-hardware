//! Desktop/server build of the listam leaf peer.
//!
//! Usage:
//!   leaf-host --connect 127.0.0.1:9991 --key <core key hex> [--control] [--storage DIR]
//!   leaf-host --listen 0.0.0.0:9990 --key <core key hex> [--control] [--storage DIR]
//!
//! `--key` may be given multiple times; pass `--control` after a key to mark
//! it as the control core (whose JSON entries announce more cores).

use std::path::PathBuf;
use std::time::Duration;

use leaf_core::{MirrorStorage, Registry, run_connection};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{error, info};

struct Args {
    connect: Vec<String>,
    listen: Option<String>,
    keys: Vec<([u8; 32], bool)>,
    storage: MirrorStorage,
    status_secs: u64,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        connect: vec![],
        listen: None,
        keys: vec![],
        storage: MirrorStorage::Memory,
        status_secs: 10,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--connect" => args.connect.push(iter.next().ok_or_else(usage)?),
            "--listen" => args.listen = Some(iter.next().ok_or_else(usage)?),
            "--key" => {
                let hex_key = iter.next().ok_or_else(usage)?;
                let bytes = hex::decode(&hex_key)?;
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("--key must be 32 bytes of hex"))?;
                args.keys.push((key, false));
            }
            "--control" => {
                let last = args
                    .keys
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("--control must follow a --key"))?;
                last.1 = true;
            }
            "--storage" => {
                args.storage = MirrorStorage::Disk(PathBuf::from(iter.next().ok_or_else(usage)?))
            }
            "--fs-storage" => {
                // Same blocking std::fs backend the ESP32 uses, for parity tests.
                args.storage = MirrorStorage::StdFs(PathBuf::from(iter.next().ok_or_else(usage)?))
            }
            "--status-secs" => args.status_secs = iter.next().ok_or_else(usage)?.parse()?,
            other => return Err(anyhow::anyhow!("unknown argument: {other}\n{}", USAGE)),
        }
    }
    if args.connect.is_empty() && args.listen.is_none() {
        return Err(anyhow::anyhow!(
            "at least one --connect or a --listen is required\n{}",
            USAGE
        ));
    }
    if args.keys.is_empty() {
        return Err(anyhow::anyhow!("at least one --key is required\n{}", USAGE));
    }
    Ok(args)
}

const USAGE: &str = "usage: leaf-host (--connect ADDR | --listen ADDR) --key HEX [--control] [--key HEX ...] [--storage DIR] [--status-secs N]";

fn usage() -> anyhow::Error {
    anyhow::anyhow!("{}", USAGE)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,leaf_core=debug".into()),
        )
        .init();
    let args = parse_args()?;

    let registry = Registry::new(args.storage.clone());
    for (key, is_control) in &args.keys {
        registry.add_core(*key, *is_control).await?;
    }
    // Re-learn cores announced in a previously synced control core.
    registry.seed_from_control().await?;

    // Periodic status line so headless operation is observable.
    {
        let registry = registry.clone();
        let every = Duration::from_secs(args.status_secs);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                for (key, length, contiguous) in registry.stats().await {
                    info!("status core={} length={length} contiguous={contiguous}", &key[..8]);
                }
            }
        });
    }

    let mut tasks = tokio::task::JoinSet::new();
    for addr in args.connect {
        // Leaf dials each hub and keeps redialing: the normal ESP32-like mode.
        let registry = registry.clone();
        tasks.spawn(async move {
            loop {
                info!("connecting to {addr} ...");
                match tokio::net::TcpStream::connect(&addr).await {
                    Ok(stream) => {
                        if let Err(err) = stream.set_nodelay(true) {
                            error!("set_nodelay: {err}");
                        }
                        info!("connected to {addr}");
                        let result =
                            run_connection(stream.compat(), true, registry.clone()).await;
                        match result {
                            Ok(()) => info!("connection to {addr} closed"),
                            Err(err) => error!("connection error: {err:#}"),
                        }
                    }
                    Err(err) => error!("could not connect to {addr}: {err}"),
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }
    if let Some(addr) = args.listen {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        info!("listening on {addr}");
        let registry = registry.clone();
        tasks.spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let _ = stream.set_nodelay(true);
                info!("connection from {peer}");
                let registry = registry.clone();
                tokio::spawn(async move {
                    match run_connection(stream.compat(), false, registry).await {
                        Ok(()) => info!("connection from {peer} closed"),
                        Err(err) => error!("connection from {peer} error: {err:#}"),
                    }
                });
            }
        });
    }
    while let Some(_result) = tasks.join_next().await {}
    Ok(())
}
