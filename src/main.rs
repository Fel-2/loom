mod app;
mod cli;
mod db;
mod git;
mod githttp;
mod ssh;
mod ui;
mod web;

use std::sync::Arc;

/// glibc gives each thread its own malloc arena and rarely returns the memory.
/// One arena keeps post-load RSS near idle (measured ~25 MB -> ~11 MB).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    const M_ARENA_MAX: libc::c_int = -8;
    unsafe { libc::mallopt(M_ARENA_MAX, 1) };
}

fn main() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    limit_malloc_arenas();

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

    // One async thread; git work goes to a small, bounded blocking pool.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(4)
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
        eprintln!("loom http on {}  (public url {}, data {})", app.http_addr, app.url, app.data.display());

        if let Some(addr) = app.ssh_addr.clone() {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = ssh::serve(app, addr).await {
                    eprintln!("ssh: {e}");
                    std::process::exit(1);
                }
            });
        }

        let router = web::router(app);
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        if let Err(e) = axum::serve(listener, router).with_graceful_shutdown(shutdown).await {
            eprintln!("serve: {e}");
        }
    });
}
