use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Result;
use arc_swap::ArcSwap;
use tokio::net::TcpListener;
use navpath_core::Snapshot;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use navpath_service::{
    build_router,
    AppState,
    SnapshotState,
    env_var,
    read_tail_hash_hex,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Parse CLI flags into the env vars the (cached, env-driven) readers in `routes.rs`
/// consume. Flags take precedence over already-set env vars; absent a flag, the env
/// var still works. Must run before the first `/route` request, which is where the
/// values are read & cached.
///
/// - `--dump-result <path>` / `--dump-result=<path>` → `NAVPATH_DUMP_RESULT`
/// - `--no-seed` → `NAVPATH_IGNORE_SEED=1`: ignore client seeds entirely — every
///   request is answered with the deterministic unseeded optimum (no edge jitter,
///   canonical pruning engages), marked `degraded: "seed_ignored"` when a seed was
///   sent (plan v3 §3b).
fn apply_cli_flags() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--no-seed" {
            std::env::set_var("NAVPATH_IGNORE_SEED", "1");
            continue;
        }
        let path = if arg == "--dump-result" {
            args.next()
        } else if let Some(rest) = arg.strip_prefix("--dump-result=") {
            Some(rest.to_string())
        } else {
            None
        };
        if let Some(path) = path {
            std::env::set_var("NAVPATH_DUMP_RESULT", path);
        }
    }
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<usize>().ok())
}

/// Explicit runtime (T3.10) instead of `#[tokio::main]`'s one-worker-per-core default:
/// - `NAVPATH_WORKER_THREADS` (default min(4, cores)): reactor work is light — JSON,
///   cache lookups, small inline hit payloads; searches run on the blocking pool.
/// - `NAVPATH_MAX_BLOCKING_THREADS` (default 2 x search permits, at least 8): searches
///   are permit-bounded, and each race holds at most two blocking threads; the rest is
///   headroom for off-reactor payload builds.
/// - `NAVPATH_BLOCKING_KEEP_ALIVE_S` (default 300): tokio's 10 s default reaps idle
///   blocking threads between low-QPS requests, so the next miss paid a fresh thread.
fn build_runtime(search_permits: usize) -> std::io::Result<tokio::runtime::Runtime> {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let workers = env_usize("NAVPATH_WORKER_THREADS").filter(|&n| n > 0).unwrap_or(cores.min(4));
    let blocking = env_usize("NAVPATH_MAX_BLOCKING_THREADS")
        .filter(|&n| n > 0)
        .unwrap_or((2 * search_permits).max(8));
    let keep_alive = std::env::var("NAVPATH_BLOCKING_KEEP_ALIVE_S").ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(300);
    info!(workers, max_blocking_threads = blocking, keep_alive_s = keep_alive, "tokio runtime");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(blocking)
        .thread_keep_alive(std::time::Duration::from_secs(keep_alive))
        .thread_name("navpath-rt")
        .enable_all()
        .build()
}

fn main() -> Result<()> {
    // Logging (T3.5): `RUST_LOG` is honoured (default `info`), and lines are handed to a
    // dedicated writer thread instead of a synchronous stdout write on a reactor thread.
    // The writer is lossy under a sustained flood rather than blocking requests. The
    // guard flushes on drop; it lives for the whole of `main`, i.e. the process.
    let (log_writer, _log_guard) = tracing_appender::non_blocking(std::io::stdout());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = FmtSubscriber::builder()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(log_writer)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    apply_cli_flags();

    let search_permits = navpath_service::default_search_permits();
    let runtime = build_runtime(search_permits.total())?;
    runtime.block_on(serve(search_permits))
}

async fn serve(search_permits: Arc<navpath_service::SearchPermits>) -> Result<()> {
    let host = env_var("NAVPATH_HOST", "127.0.0.1");
    let port: u16 = env_var("NAVPATH_PORT", "8080").parse().unwrap_or(8080);
    let snapshot_path = PathBuf::from(env_var("SNAPSHOT_PATH", "./graph.snapshot"));

    let init = match Snapshot::open(&snapshot_path) {
        Ok(s) => {
            let path = snapshot_path.clone();
            // Hash of the MAPPED file (re-reading the path could see a newer file).
            let hash_hex = s.tail_hash_hex();
            // The load-time builders run on scoped threads (see SnapshotState::build);
            // keep them off the reactor.
            tokio::task::spawn_blocking(move || SnapshotState::build(path, s, hash_hex)).await?
        }
        Err(e) => {
            // Provide not-ready state if snapshot failed to load
            error!(error=?e, path=?snapshot_path, "failed to open snapshot; service will still start but /route will 503");
            SnapshotState::unloaded(snapshot_path.clone(), read_tail_hash_hex(&snapshot_path))
        }
    };
    let state = AppState {
        current: Arc::new(ArcSwap::from_pointee(init)),
        search_permits,
        metrics: Arc::new(navpath_service::Metrics::default()),
        ctx_pool: navpath_service::ContextPool::new(),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    // Warm-up off the reactor: populate (and optionally mlock) the snapshot mapping,
    // then pre-allocate + page in the search context pool. The listener is bound
    // first so `/health` can report `ready: false` meanwhile; `/route` answers 503
    // until this flips the flag.
    {
        let state = state.clone();
        std::thread::Builder::new().name("navpath-warmup".into()).spawn(move || {
            let t = std::time::Instant::now();
            let cur = state.current.load();
            if let Some(snap) = cur.snapshot.as_ref() {
                cur.set_warm_state(navpath_service::warm_snapshot(snap));
                // `NAVPATH_CTX_PREWARM` counts search CONTEXTS (~19 MB each at 1.1M
                // nodes; a bidirectional search uses two, anything else one). Default:
                // two per search permit (three with the engine race on: a race runs a
                // one-context and a two-context search), capped at 16 — the same memory
                // as the old default of 8 context pairs. Contexts beyond the concurrent-
                // miss count are idle memory the kernel reclaims first under pressure,
                // and measured on a swapping host the 64-pair (2.3 GB) warm-up produced
                // 50-170 ms first-request stalls that 8 pairs (0.3 GB) did not. Raise
                // it on a dedicated box (0 disables); `/stats` `ctx_pool.fresh_allocations`
                // counts the contexts requests had to allocate themselves.
                let nodes = snap.counts().nodes as usize;
                let permits = state.search_permits.total();
                let per_search = if navpath_service::routes::race_enabled() { 3 } else { 2 };
                let default_contexts = (permits * per_search).min(16);
                let contexts = std::env::var("NAVPATH_CTX_PREWARM").ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(default_contexts);
                let tp = std::time::Instant::now();
                state.ctx_pool.prewarm(contexts, nodes);
                info!(contexts, elapsed_ms = tp.elapsed().as_millis() as u64, "pre-warmed search context pool");
            }
            state.ready.store(true, std::sync::atomic::Ordering::Release);
            info!(elapsed_ms = t.elapsed().as_millis() as u64, "warm-up complete; serving routes");
            // Keep whatever could not be mlock'd resident (NAVPATH_KEEP_WARM_S).
            if navpath_service::keep_warm_disabled_by_populate() {
                return;
            }
            if let Some(interval) = navpath_service::keep_warm_interval() {
                navpath_service::spawn_keep_warm(state.current.clone(), interval);
                info!(interval_s = interval.as_secs(), "keep-warm loop started");
            }
        }).expect("spawn warm-up thread");
    }
    let app = build_router(state.clone());
    let addr: SocketAddr = format!("{}:{}", host, port).parse().unwrap();
    if let Ok(dump) = std::env::var("NAVPATH_DUMP_RESULT") {
        if !dump.trim().is_empty() {
            info!(path = %dump, "result dumping enabled; each /route response overwrites this file");
        }
    }
    if navpath_service::routes::seeding_disabled() {
        info!("seeding disabled (--no-seed): client seeds are ignored; all searches run unseeded");
    }
    if navpath_service::routes::race_enabled() {
        info!(
            primary = navpath_service::routes::race_primary().as_str(),
            hedge_ms = navpath_service::routes::race_hedge_delay().as_secs_f64() * 1000.0,
            gate = navpath_service::routes::race_gate(),
            "engine race enabled"
        );
    }
    info!(%addr, path=?snapshot_path, "starting navpath-service");
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service())
        .tcp_nodelay(true)
        .await?;
    Ok(())
}
