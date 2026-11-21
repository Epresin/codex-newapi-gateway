use anyhow::{Result, bail};
use codex_newapi_gateway::{
    config::{Config, Store},
    models_dev,
    proxy::{self, ProxyState},
};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const USAGE: &str = "Usage: codex-newapi-gateway [--config <path> | -c <path>] [--version | -V]";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = match args.next().as_deref() {
        Some("--config" | "-c") => PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?,
        ),
        Some("--version" | "-V") => {
            println!("codex-newapi-gateway {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        None => std::env::var_os("GATEWAY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("config.toml")),
        _ => bail!("{USAGE}"),
    };
    if args.next().is_some() {
        bail!("Unexpected command argument");
    }
    let cfg = Config::load(&path)?;
    let store = Store {
        dir: cfg.metadata.cache_dir.clone(),
    };
    let db = if cfg.metadata.enabled {
        models_dev::cached(&store)
    } else {
        Default::default()
    };
    let state = ProxyState::new(cfg.clone(), db)?;
    if cfg.metadata.enabled {
        let shared = state.clone();
        let metadata = cfg.metadata.clone();
        tokio::spawn(async move {
            loop {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let result = tokio::select! {
                    _ = shared.cancel.cancelled() => break,
                    result = models_dev::fetch(&store, &metadata, now, true) => result,
                };
                match result {
                    Ok(db) => {
                        eprintln!(
                            "metadata records={} stale={}",
                            db.records.len(),
                            db.warning.is_some()
                        );
                        *shared.metadata.write().unwrap() = db;
                    }
                    Err(_) => eprintln!("metadata refresh failed; keeping last good data"),
                }
                tokio::select! {
                    _ = shared.cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(metadata.refresh_interval_secs)) => {},
                }
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(cfg.server.listen).await?;
    eprintln!(
        "codex-newapi-gateway {} listening={}",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?
    );
    let cancel = state.cancel.clone();
    let shutdown = async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        cancel.cancel();
    };
    axum::serve(listener, proxy::router(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
