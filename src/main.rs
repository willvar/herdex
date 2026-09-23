//! herdex — codex-only account pool gateway (Rust).
//!
//! The TLS stack is pinned to the versions used by codex-rs (reqwest 0.12.28,
//! rustls 0.23.36, aws-lc-rs) so the outbound fingerprint matches the
//! official client.

use clap::Parser as _;
use herdex::app::{spawn_refresh_loop, App, AppHandle};
use herdex::config;
use herdex::pool;
use herdex::store;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(clap::Parser)]
struct Args {
    /// Path to the YAML config file
    #[arg(short = 'c', long, default_value = "/etc/herdex/herdex.toml")]
    config: String,
}

fn main() {
    // the rustls dependency tree carries both aws-lc-rs and ring features;
    // rustls cannot auto-pick a process-level CryptoProvider in that case
    // and panics on first use — install ours explicitly, before any TLS.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("install rustls crypto provider");

    let args = Args::parse();

    let cfg = match config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };
    let level = if cfg.log.level.is_empty() {
        "info".into()
    } else {
        cfg.log.level.clone()
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async_main(cfg));
}

async fn async_main(cfg: config::Config) {
    let store = match store::Store::open(&cfg.state_root) {
        Ok(s) => s,
        Err(e) => {
            log::error!("store: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = store.get_settings() {
        log::error!("settings: {e}");
        std::process::exit(1);
    }
    let pool = pool::Pool::new(store.clone());

    let usage_root = {
        let base = cfg.upstream.base_url.trim_end_matches('/');
        match url::Url::parse(base) {
            Ok(u) => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("chatgpt.com")),
            Err(_) => "https://chatgpt.com".into(),
        }
    };

    let app: AppHandle = Arc::new(App {
        cfg: cfg.clone(),
        store: store.clone(),
        pool,
        http: herdex::app::build_http_client(),
        usage_root,
        pending: Mutex::new(HashMap::new()),
        refresh_guards: tokio::sync::Mutex::new(HashMap::new()),
        last_prune_day: std::sync::atomic::AtomicI64::new(0),
        learned_strips: std::sync::Mutex::new(herdex::app::App::load_learned_strips(&cfg)),
    });

    spawn_refresh_loop(app.clone());

    let root = herdex::app::build_router(app.clone());

    log::info!(
        "herdex listening on {} (state: {})",
        cfg.listen,
        cfg.state_root
    );
    // TLS is unconditional: herdex serves codex, and codex 0.156+ only
    // accepts HTTPS backends (workspace routing rejects plain HTTP)
    let acceptor = match herdex::tls::server_config(&cfg.state_root, &cfg.tls) {
        Ok(server) => tokio_rustls::TlsAcceptor::from(server),
        Err(e) => {
            log::error!("tls: {e}");
            std::process::exit(1);
        }
    };
    log::info!(
        "herdex serving HTTPS on {} (CA: {}/herdex-ca.pem)",
        cfg.listen,
        cfg.state_root
    );
    let listener = match tokio::net::TcpListener::bind(&cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("bind {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    let app = hyper_util::service::TowerToHyperService::new(root.into_service());
    loop {
        let (tcp, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                log::error!("accept: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else {
                return;
            };
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), app)
                    .await;
        });
    }
}
