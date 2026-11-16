//! loadgen — fixed-work HTTP benchmark with process-tree memory sampling.
//!
//!   loadgen run  --urls FILE --conc N --reps N [--warmup N] --pid PID
//!   loadgen idle --pid PID --secs N
//!
//! URLS file: one `label<TAB>url` per line. `run` warms up, then issues every
//! URL `reps` times (interleaved) with N concurrent keep-alive clients, and
//! samples RSS of PID and all its descendants every 50 ms.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// (pid, ppid) for every process.
fn proc_table() -> Vec<(u32, u32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // ppid is the 2nd field after the ")" closing the comm.
            let rest = &stat[stat.rfind(')')? + 2..];
            let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, ppid))
        })
        .collect()
}

fn rss_kb(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

/// (root RSS, root + descendants RSS, descendant count) in KB.
fn tree_rss(root: u32) -> (u64, u64, usize) {
    let table = proc_table();
    let mut stack = vec![root];
    let (mut total, mut n) = (0, 0);
    while let Some(p) = stack.pop() {
        total += rss_kb(p);
        for (c, _) in table.iter().filter(|(_, pp)| *pp == p) {
            stack.push(*c);
            n += 1;
        }
    }
    (rss_kb(root), total, n)
}

#[derive(Default)]
struct Peak {
    root: u64,
    tree: u64,
    children: usize,
}

fn sampler(pid: u32, stop: Arc<AtomicBool>, peak: Arc<Mutex<Peak>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let (r, t, n) = tree_rss(pid);
            let mut p = peak.lock().unwrap();
            p.root = p.root.max(r);
            p.tree = p.tree.max(t);
            p.children = p.children.max(n);
            drop(p);
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

async fn run(args: &[String]) {
    let urls_file = arg(args, "--urls").expect("--urls");
    let conc: usize = arg(args, "--conc").map_or(32, |v| v.parse().unwrap());
    let reps: usize = arg(args, "--reps").map_or(20, |v| v.parse().unwrap());
    let warmup: usize = arg(args, "--warmup").map_or(2, |v| v.parse().unwrap());
    let pid: u32 = arg(args, "--pid").expect("--pid").parse().unwrap();
    let urls: Vec<(String, String)> = std::fs::read_to_string(&urls_file)
        .expect("read urls")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (a, b) = l.split_once('\t').expect("label<TAB>url");
            (a.to_owned(), b.to_owned())
        })
        .collect();

    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).pool_max_idle_per_host(conc * 2).build_http();
    let fetch = |client: Client<hyper_util::client::legacy::connect::HttpConnector, Empty<Bytes>>, url: String| async move {
        let t = Instant::now();
        let res = client.get(url.parse().unwrap()).await;
        match res {
            Ok(r) => {
                let status = r.status().as_u16();
                let len = r.into_body().collect().await.map(|b| b.to_bytes().len()).unwrap_or(0);
                (t.elapsed().as_secs_f64() * 1000.0, status, len)
            }
            Err(_) => (t.elapsed().as_secs_f64() * 1000.0, 0, 0),
        }
    };

    for _ in 0..warmup {
        for (_, u) in &urls {
            fetch(client.clone(), u.clone()).await;
        }
    }

    // Interleave labels so no endpoint gets a quiet or a busy phase to itself.
    let jobs: Arc<Vec<(usize, String)>> =
        Arc::new((0..reps).flat_map(|_| urls.iter().enumerate().map(|(i, (_, u))| (i, u.clone()))).collect());
    let next = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<(usize, f64, u16, usize)>>> = Arc::new(Mutex::new(Vec::with_capacity(jobs.len())));

    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(Mutex::new(Peak::default()));
    let sam = sampler(pid, stop.clone(), peak.clone());
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..conc {
        let (jobs, next, results, client) = (jobs.clone(), next.clone(), results.clone(), client.clone());
        handles.push(tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((label, url)) = jobs.get(i) else { break };
                let (ms, st, len) = fetch(client.clone(), url.clone()).await;
                results.lock().unwrap().push((*label, ms, st, len));
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let wall = start.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let _ = sam.join();

    let results = results.lock().unwrap();
    let mut by: HashMap<usize, Vec<f64>> = HashMap::new();
    let mut errors: HashMap<usize, usize> = HashMap::new();
    let mut bytes: HashMap<usize, usize> = HashMap::new();
    for (l, ms, st, len) in results.iter() {
        by.entry(*l).or_default().push(*ms);
        if !(200..400).contains(st) {
            *errors.entry(*l).or_default() += 1;
        }
        bytes.insert(*l, *len);
    }
    println!("{:<22} {:>9} {:>9} {:>9} {:>9} {:>6} {:>9}", "page", "p50 ms", "p95 ms", "p99 ms", "max ms", "errs", "bytes");
    for (i, (label, _)) in urls.iter().enumerate() {
        let mut v = by.remove(&i).unwrap_or_default();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{:<22} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>6} {:>9}",
            label,
            pct(&v, 0.50),
            pct(&v, 0.95),
            pct(&v, 0.99),
            v.last().copied().unwrap_or(0.0),
            errors.get(&i).copied().unwrap_or(0),
            bytes.get(&i).copied().unwrap_or(0)
        );
    }
    let p = peak.lock().unwrap();
    println!(
        "TOTAL requests={} wall={:.2}s rps={:.0} peak_rss_kb={} peak_tree_rss_kb={} max_children={}",
        results.len(),
        wall,
        results.len() as f64 / wall,
        p.root,
        p.tree,
        p.children
    );
}

fn idle(args: &[String]) {
    let pid: u32 = arg(args, "--pid").expect("--pid").parse().unwrap();
    let secs: u64 = arg(args, "--secs").map_or(10, |v| v.parse().unwrap());
    let (mut min, mut max, mut sum, mut n) = (u64::MAX, 0, 0u64, 0u64);
    let mut tree_max = 0;
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        let (r, t, _) = tree_rss(pid);
        min = min.min(r);
        max = max.max(r);
        sum += r;
        n += 1;
        tree_max = tree_max.max(t);
        std::thread::sleep(Duration::from_millis(200));
    }
    let threads = std::fs::read_dir(format!("/proc/{pid}/task")).map(|d| d.count()).unwrap_or(0);
    println!("IDLE rss_kb min={min} avg={} max={max} tree_max_kb={tree_max} threads={threads}", sum / n.max(1));
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => run(&args).await,
        Some("idle") => idle(&args),
        _ => eprintln!("usage: loadgen run --urls F --conc N --reps N [--warmup N] --pid P | loadgen idle --pid P --secs N"),
    }
}
