mod api;
mod app;
mod cli;
mod db;
mod events;
mod feeds;
mod git;
mod githttp;
mod gitops;
mod highlight;
mod mirrors;
mod hooks;
mod issues_db;
mod issues_web;
mod lfs;
mod ops;
mod pulls_db;
mod pulls_web;
mod releases;
mod refs;
mod social_db;
mod social_web;
mod totp;
mod ssh;
mod ui;
mod ui_issues;
mod ui_pulls;
mod ui_social;
mod web;

use std::sync::Arc;

/// glibc gives each thread its own malloc arena and rarely returns the memory.
/// One arena keeps post-load RSS near idle (measured ~25 MB -> ~11 MB).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    const M_ARENA_MAX: libc::c_int = -8;
    unsafe { libc::mallopt(M_ARENA_MAX, 1) };
}

/// After a burst, glibc keeps freed heap pages. Hand them back once a minute
/// (measured after browse load: 17.6 MB -> 12.1 MB, heap 1.7 MB). Costs microseconds.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
async fn trim_heap_periodically() {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        unsafe { libc::malloc_trim(0) };
    }
}

/// Blocking threads for git work (LOOM_GIT_THREADS, default 4). Measured: more
/// threads buy little throughput but cost ~15 MB peak each under load and slow
/// the cheap pages (bench/results/SUMMARY.md), so the default stays small.
fn git_threads() -> usize {
    std::env::var("LOOM_GIT_THREADS").ok().and_then(|v| v.parse().ok()).filter(|n: &usize| (1..=256).contains(n)).unwrap_or(4)
}

fn main() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    limit_malloc_arenas();
    // Die quietly on a closed pipe like other Unix tools, instead of the
    // panic `println!` raises when stdout goes away (e.g. `loom lfs gc | head`).
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = cli::run(&args) {
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }

    let app = match app::App::from_env() {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = app.install_hooks() {
        eprintln!("install hooks: {e}");
        std::process::exit(1);
    }

    // One async thread; git work goes to a bounded blocking pool whose
    // threads exit after 10 s idle, so idle stays at one thread.
    let git_threads = git_threads();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(git_threads)
        .thread_keep_alive(std::time::Duration::from_secs(10))
        .build()
        .expect("runtime");

    rt.block_on(async {
        let listener = match tokio::net::TcpListener::bind(&app.http_addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bind {}: {e}", app.http_addr);
                std::process::exit(1);
            }
        };
        eprintln!("loom http on {}  (public url {}, data {}, git threads {git_threads})", app.http_addr, app.url, app.data.display());

        if let Some(addr) = app.ssh_addr.clone() {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = ssh::serve(app, addr).await {
                    eprintln!("ssh: {e}");
                    std::process::exit(1);
                }
            });
        }

        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        tokio::spawn(trim_heap_periodically());

        tokio::spawn(events::run_deliveries(app.clone()));
        tokio::spawn(mirrors::run(app.clone()));

        let router = web::router(app);
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        let service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        if let Err(e) = axum::serve(listener, service).with_graceful_shutdown(shutdown).await {
            eprintln!("serve: {e}");
        }
    });
}
