//! Admin commands. Safe to run while the server is up (SQLite WAL).

use std::io::Read;

use crate::app::App;
use crate::db;

const USAGE: &str = "\
usage: loom [serve]
       loom user add <name> [--admin] [--email <e>] [--password-stdin]
       loom user passwd <name> [--password-stdin]
       loom user del <name>
       loom user list
       loom user 2fa-off <name>   (recover an account that lost its authenticator)
       loom key add <user> <pubkey-file|-> [title]
       loom repo create <owner>/<name> [--private] [--desc <text>]
       loom repo adopt        register bare repos already on disk (public)
       loom repo list
       loom version
       loom doctor               check this machine is ready to run loom
       loom backup <dir> [--repos] [--keep N]
       loom restore <backup.db> [--repos <dir>] [--force]
       loom mail flush          (send queued notifications via LOOM_MAIL_CMD)
       loom lfs gc [--dry-run] [--repo owner/name]
       loom repo maintain     write commit-graphs for all repos (speeds up history)
       loom org create <name> <admin-user>
       loom org add <org> <user> [read|write|admin]

env: LOOM_DATA (./data)  LOOM_ADDR (127.0.0.1:3000)  LOOM_URL
     LOOM_SSH_ADDR (127.0.0.1:2222, 'off' disables)  LOOM_SSH_HOST";

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn read_password(args: &[String]) -> Result<String, String> {
    if flag(args, "--password-stdin") {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
        let pw = s.trim_end_matches(['\n', '\r']).to_owned();
        return check_pw(pw);
    }
    let a = rpassword::prompt_password("password: ").map_err(|e| e.to_string())?;
    let b = rpassword::prompt_password("again: ").map_err(|e| e.to_string())?;
    if a != b {
        return Err("passwords don't match".into());
    }
    check_pw(a)
}

fn check_pw(pw: String) -> Result<String, String> {
    if pw.chars().count() < 8 {
        return Err("password must be at least 8 characters".into());
    }
    Ok(pw)
}

/// Returns `None` when the command is `serve` (or empty).
pub fn run(args: &[String]) -> Option<Result<(), String>> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    if a.is_empty() || a[0] == "serve" {
        return None;
    }
    if a[0] == "version" {
        let git = std::process::Command::new("git").arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
        println!("loom {} ({git})", env!("CARGO_PKG_VERSION"));
        return Some(Ok(()));
    }
    if a[0] == "backup" {
        return Some(backup(a.get(1).copied(), &args[2..]));
    }
    if a[0] == "doctor" {
        return Some(doctor());
    }
    if a[0] == "restore" {
        return Some(restore(a.get(1).copied(), &args[2..]));
    }
    if a[0] == "hook" {
        return Some(match a.get(1).copied() {
            Some("pre-receive") => hook_pre_receive(),
            Some("post-receive") => hook_post_receive(),
            _ => Err("unknown hook".into()),
        });
    }
    if matches!(a[0], "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Some(Ok(()));
    }
    let app = match App::from_env() {
        Ok(a) => a,
        Err(e) => return Some(Err(e)),
    };
    let rest = &args[2.min(args.len())..];
    Some(match (a[0], a.get(1).copied()) {
        ("user", Some("add")) => user_add(&app, rest),
        ("user", Some("passwd")) => user_passwd(&app, rest),
        ("user", Some("del")) => user_del(&app, rest),
        ("user", Some("list")) => user_list(&app),
        ("user", Some("2fa-off")) => user_2fa_off(&app, rest),
        ("key", Some("add")) => key_add(&app, rest),
        ("repo", Some("create")) => repo_create(&app, rest),
        ("repo", Some("adopt")) => repo_adopt(&app),
        ("repo", Some("list")) => repo_list(&app),
        ("repo", Some("maintain")) => repo_maintain(&app),
        ("lfs", Some("gc")) => {
            let dry = flag(rest, "--dry-run");
            let only = opt(rest, "--repo").map(str::to_owned);
            crate::lfs::gc(&app, dry, only.as_deref()).and_then(|(n, b)| {
                println!("{}{n} objects, {} bytes", if dry { "would remove " } else { "removed " }, b);
                Ok(())
            })
        }
        ("mail", Some("flush")) => {
            let (sent, failed) = crate::events::flush_mail(&app.db, 200);
            println!("mail: {sent} sent, {failed} failed");
            Ok(())
        }
        ("org", Some("create")) => org_create(&app, rest),
        ("org", Some("add")) => org_add(&app, rest),
        _ => Err(USAGE.into()),
    })
}

/// Runs inside `git receive-pack` (cwd = repo, objects quarantined). Enforces
/// branch protection. Exit status non-zero rejects the whole push.
fn hook_pre_receive() -> Result<(), String> {
    use std::io::BufRead;
    let data = std::env::var_os("LOOM_DATA").ok_or("LOOM_DATA not set")?;
    let repo_id: i64 = std::env::var("LOOM_REPO_ID").ok().and_then(|v| v.parse().ok()).ok_or("LOOM_REPO_ID not set")?;
    let db = db::Db::open(&std::path::Path::new(&data).join("loom.db")).map_err(|e| e.to_string())?;
    let rules = db.protections(repo_id).map_err(|e| e.to_string())?;
    let zero = |s: &str| s.bytes().all(|b| b == b'0');
    let mut errors = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let mut it = line.split_whitespace();
        let (Some(old), Some(new), Some(refname)) = (it.next(), it.next(), it.next()) else { continue };
        // PR heads are maintained by loom; a push there could spoof a PR's contents.
        if refname.starts_with("refs/pull/") {
            errors.push(format!("{refname} is managed by loom"));
            continue;
        }
        let Some(branch) = refname.strip_prefix("refs/heads/") else { continue };
        let Some(rule) = rules.iter().find(|r| r.branch == branch) else { continue };
        if rule.require_pr && !zero(new) {
            errors.push(format!("branch '{branch}' only accepts changes through pull requests"));
            continue;
        }
        if zero(new) {
            if !rule.allow_delete {
                errors.push(format!("branch '{branch}' is protected: deletion not allowed"));
            }
        } else if !zero(old) && !rule.allow_force_push {
            let ff = std::process::Command::new("git")
                .args(["merge-base", "--is-ancestor", old, new])
                .status()
                .map_err(|e| e.to_string())?;
            if !ff.success() {
                errors.push(format!("branch '{branch}' is protected: force-push not allowed"));
            }
        }
    }
    if errors.is_empty() {
        return Ok(());
    }
    Err(errors.iter().map(|e| format!("loom: {e}")).collect::<Vec<_>>().join("\n"))
}

/// Runs after a successful push; see `hooks::after_update`. Never fails the
/// push: problems are only printed.
fn hook_post_receive() -> Result<(), String> {
    use std::io::BufRead;
    let data = std::path::PathBuf::from(std::env::var_os("LOOM_DATA").ok_or("LOOM_DATA not set")?);
    let repo_id: i64 = std::env::var("LOOM_REPO_ID").ok().and_then(|v| v.parse().ok()).ok_or("LOOM_REPO_ID not set")?;
    let db = db::Db::open(&data.join("loom.db")).map_err(|e| e.to_string())?;
    let repo = db.repo_by_id(repo_id).map_err(|e| e.to_string())?.ok_or("repo not found")?;
    let pusher = std::env::var("LOOM_USER").ok().and_then(|n| db.user_by_name(&n).ok().flatten());
    let updates: Vec<crate::hooks::Update> = std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some(crate::hooks::Update { old: it.next()?.to_owned(), new: it.next()?.to_owned(), refname: it.next()?.to_owned() })
        })
        .collect();
    let url = std::env::var("LOOM_URL").unwrap_or_default();
    crate::hooks::after_update(&db, &data, &url, &repo, pusher.as_ref(), &updates);
    Ok(())
}

fn user_add(app: &App, args: &[String]) -> Result<(), String> {
    let name = args.first().ok_or(USAGE)?;
    if !db::valid_name(name) {
        return Err("invalid user name".into());
    }
    let pw = read_password(args)?;
    let email = opt(args, "--email").unwrap_or("");
    app.db.create_user(name, email, Some(&pw), flag(args, "--admin")).map_err(|e| e.to_string())?;
    println!("created user {name}{}", if flag(args, "--admin") { " (admin)" } else { "" });
    Ok(())
}

fn user_passwd(app: &App, args: &[String]) -> Result<(), String> {
    let name = args.first().ok_or(USAGE)?;
    let u = app.db.user_by_name(name).map_err(|e| e.to_string())?.ok_or("no such user")?;
    let pw = read_password(args)?;
    app.db.set_password(u.id, &pw).map_err(|e| e.to_string())?;
    println!("password updated; {name}'s sessions were signed out");
    Ok(())
}

fn user_del(app: &App, args: &[String]) -> Result<(), String> {
    let name = args.first().ok_or(USAGE)?;
    if !app.db.delete_user(name).map_err(|e| e.to_string())? {
        return Err("no such user".into());
    }
    println!("deleted user {name} (their repositories stay on disk)");
    Ok(())
}

fn user_2fa_off(app: &App, args: &[String]) -> Result<(), String> {
    let name = args.first().ok_or(USAGE)?;
    let u = app.db.user_by_name(name).map_err(|e| e.to_string())?.ok_or("no such user")?;
    app.db.set_totp(u.id, "", false).map_err(|e| e.to_string())?;
    println!("two-factor disabled for {name}; their sessions were NOT signed out");
    Ok(())
}

fn user_list(app: &App) -> Result<(), String> {
    for u in app.db.users().map_err(|e| e.to_string())? {
        println!("{}{}{}", u.name, if u.is_admin { "  admin" } else { "" }, if u.email.is_empty() { String::new() } else { format!("  <{}>", u.email) });
    }
    Ok(())
}

/// Parse an OpenSSH public key line; returns (normalized key, SHA256 fingerprint).
pub fn parse_pubkey(line: &str) -> Result<(String, String), String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim()).map_err(|e| format!("invalid public key: {e}"))?;
    let fp = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
    let mut norm = key.clone();
    norm.set_comment("");
    Ok((norm.to_openssh().map_err(|e| e.to_string())?, fp))
}

fn key_add(app: &App, args: &[String]) -> Result<(), String> {
    let (user, src) = (args.first().ok_or(USAGE)?, args.get(1).ok_or(USAGE)?);
    let u = app.db.user_by_name(user).map_err(|e| e.to_string())?.ok_or("no such user")?;
    let text = if src == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
        s
    } else {
        std::fs::read_to_string(src).map_err(|e| format!("{src}: {e}"))?
    };
    let (key, fp) = parse_pubkey(&text)?;
    let title = args.get(2).cloned().unwrap_or_else(|| text.split_whitespace().nth(2).unwrap_or("key").to_owned());
    app.db.add_key(u.id, &title, &fp, &key).map_err(|e| e.to_string())?;
    println!("added {fp} for {}", u.name);
    Ok(())
}

fn repo_create(app: &App, args: &[String]) -> Result<(), String> {
    let full = args.first().ok_or(USAGE)?;
    let (owner, name) = full.split_once('/').ok_or("expected <owner>/<name>")?;
    let r = app.create_repo(None, owner, name, opt(args, "--desc").unwrap_or(""), flag(args, "--private"))?;
    let (http, ssh) = app.clone_urls(&r);
    println!("created {}/{}{}\n  {http}", r.owner, r.name, if r.private { " (private)" } else { "" });
    if let Some(s) = ssh {
        println!("  {s}");
    }
    Ok(())
}

fn repo_adopt(app: &App) -> Result<(), String> {
    let owners = std::fs::read_dir(&app.repos).map_err(|e| e.to_string())?;
    let mut n = 0;
    for o in owners.flatten() {
        let owner = o.file_name().to_string_lossy().into_owned();
        let Ok(repos) = std::fs::read_dir(o.path()) else { continue };
        for r in repos.flatten() {
            let fname = r.file_name().to_string_lossy().into_owned();
            let Some(name) = fname.strip_suffix(".git") else { continue };
            if app.db.repo(&owner, name).map_err(|e| e.to_string())?.is_some() {
                continue;
            }
            if app.db.user_by_name(&owner).map_err(|e| e.to_string())?.is_none() {
                eprintln!("skip {owner}/{name}: no user '{owner}' (create it first)");
                continue;
            }
            let desc = std::fs::read_to_string(r.path().join("description"))
                .ok()
                .map(|d| d.trim().to_owned())
                .filter(|d| !d.starts_with("Unnamed repository"))
                .unwrap_or_default();
            match app.create_repo(None, &owner, name, &desc, false) {
                Ok(_) => {
                    println!("adopted {owner}/{name}");
                    n += 1;
                }
                Err(e) => eprintln!("skip {owner}/{name}: {e}"),
            }
        }
    }
    println!("{n} repositories adopted");
    Ok(())
}

fn repo_maintain(app: &App) -> Result<(), String> {
    let admin = db::User { id: 0, name: String::new(), email: String::new(), is_admin: true, unread: 0 };
    for r in app.db.visible_repos(Some(&admin), None).map_err(|e| e.to_string())? {
        let t = std::time::Instant::now();
        match crate::gitops::write_commit_graph(&app.repo_dir(&r)) {
            Ok(()) => println!("{}/{}: commit-graph ok ({} ms)", r.owner, r.name, t.elapsed().as_millis()),
            Err(e) => eprintln!("{}/{}: {e}", r.owner, r.name),
        }
    }
    Ok(())
}

fn repo_list(app: &App) -> Result<(), String> {
    let admin = db::User { id: 0, name: String::new(), email: String::new(), is_admin: true, unread: 0 };
    for r in app.db.visible_repos(Some(&admin), None).map_err(|e| e.to_string())? {
        println!("{}/{}{}", r.owner, r.name, if r.private { "  private" } else { "" });
    }
    Ok(())
}

pub fn parse_role(s: &str) -> Option<db::Access> {
    match s {
        "read" => Some(db::Access::Read),
        "write" => Some(db::Access::Write),
        "admin" => Some(db::Access::Admin),
        _ => None,
    }
}

fn org_create(app: &App, args: &[String]) -> Result<(), String> {
    let (name, admin) = (args.first().ok_or(USAGE)?, args.get(1).ok_or(USAGE)?);
    if !db::valid_name(name) {
        return Err("invalid org name".into());
    }
    let u = app.db.user_by_name(admin).map_err(|e| e.to_string())?.ok_or("no such user")?;
    app.db.create_org(name, "", u.id).map_err(|e| e.to_string())?;
    println!("created org {name} (admin: {})", u.name);
    Ok(())
}

fn org_add(app: &App, args: &[String]) -> Result<(), String> {
    let (org, user) = (args.first().ok_or(USAGE)?, args.get(1).ok_or(USAGE)?);
    let role = parse_role(args.get(2).map_or("write", String::as_str)).ok_or("role must be read, write or admin")?;
    let o = app.db.owner(org).map_err(|e| e.to_string())?.filter(|o| o.is_org).ok_or("no such org")?;
    let u = app.db.user_by_name(user).map_err(|e| e.to_string())?.ok_or("no such user")?;
    app.db.set_member(o.id, u.id, role).map_err(|e| e.to_string())?;
    println!("{} is now {:?} in {}", u.name, role, o.name);
    Ok(())
}

fn backup(dir: Option<&str>, args: &[String]) -> Result<(), String> {
    let dir = dir.ok_or("usage: loom backup <dir> [--repos]")?;
    let data = std::path::PathBuf::from(std::env::var_os("LOOM_DATA").unwrap_or_else(|| "./data".into()));
    let dest = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dest).map_err(|e| format!("{dir}: {e}"))?;
    let db = db::Db::open(&data.join("loom.db")).map_err(|e| e.to_string())?;
    let stamp = db::now();
    let file = dest.join(format!("loom-{stamp}.db"));
    db.backup_to(&file).map_err(|e| e.to_string())?;
    println!("database -> {}", file.display());
    let keep: Option<usize> = opt(args, "--keep").and_then(|v| v.parse().ok());
    if args.iter().any(|a| a == "--repos") {
        let out = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(data.join("repos"))
            .arg(dest.join(format!("repos-{stamp}")))
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        println!("repositories -> {}", dest.join(format!("repos-{stamp}")).display());
    } else {
        println!("(metadata only; add --repos to copy the repositories too)");
    }
    if let Some(n) = keep.filter(|n| *n > 0) {
        rotate(&dest, n)?;
    }
    Ok(())
}

/// Keep the newest `n` backup sets (a loom-<stamp>.db and its repos-<stamp>).
fn rotate(dest: &std::path::Path, n: usize) -> Result<(), String> {
    use std::collections::BTreeMap;
    let mut sets: BTreeMap<String, (Option<std::path::PathBuf>, Option<std::path::PathBuf>)> = BTreeMap::new();
    for e in std::fs::read_dir(dest).map_err(|e| e.to_string())?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let stamp = name
            .strip_prefix("loom-")
            .and_then(|r| r.strip_suffix(".db"))
            .or_else(|| name.strip_prefix("repos-"))
            .map(str::to_owned);
        if let Some(st) = stamp {
            let slot = sets.entry(st).or_default();
            if name.starts_with("loom-") {
                slot.0 = Some(e.path());
            } else {
                slot.1 = Some(e.path());
            }
        }
    }
    let excess = sets.len().saturating_sub(n);
    for (_, (db, repos)) in sets.into_iter().take(excess) {
        for p in [db, repos].into_iter().flatten() {
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
            println!("rotated out {}", p.display());
        }
    }
    Ok(())
}

/// Pre-flight checks for a new machine; never modifies anything.
fn doctor() -> Result<(), String> {
    let data = std::path::PathBuf::from(std::env::var_os("LOOM_DATA").unwrap_or_else(|| "./data".into()));
    let mut bad = 0;
    let mut say = |ok: bool, label: &str, detail: String| {
        println!("{} {label:<22} {detail}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            bad += 1;
        }
    };
    say(true, "version", format!("loom {} ({})", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH));
    match std::process::Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => say(true, "git", String::from_utf8_lossy(&o.stdout).trim().to_owned()),
        _ => say(false, "git", "not found in PATH (git is required for clones, pushes, merges)".into()),
    }
    let writable = std::fs::create_dir_all(&data).is_ok()
        && std::fs::write(data.join(".doctor"), b"x").is_ok()
        && std::fs::remove_file(data.join(".doctor")).is_ok();
    say(writable, "data directory", format!("{} ({})", data.display(), if writable { "writable" } else { "NOT writable" }));
    if let Ok(v) = std::fs::metadata(&data) {
        let _ = v;
        if let Ok(out) = std::process::Command::new("df").args(["-Pk"]).arg(&data).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().nth(1) {
                let free: u64 = line.split_whitespace().nth(3).and_then(|v| v.parse().ok()).unwrap_or(0);
                say(free > 1024 * 1024, "free space", format!("{:.1} GB under {}", free as f64 / 1048576.0, data.display()));
            }
        }
    }
    let addr = std::env::var("LOOM_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let free = std::net::TcpListener::bind(&addr).is_ok();
    say(free, "http address", format!("{addr} is {}", if free { "free" } else { "IN USE by something else" }));
    let ssh = std::env::var("LOOM_SSH_ADDR").unwrap_or_else(|_| "127.0.0.1:2222".into());
    if ssh == "off" {
        say(true, "ssh address", "disabled".into());
    } else {
        let free = std::net::TcpListener::bind(&ssh).is_ok();
        say(free, "ssh address", format!("{ssh} is {}", if free { "free" } else { "IN USE by something else" }));
    }
    match std::env::var("LOOM_URL") {
        Ok(u) if u.starts_with("http://") || u.starts_with("https://") => say(true, "public url", u),
        Ok(u) => say(false, "public url", format!("{u} should start with http:// or https://")),
        Err(_) => say(true, "public url", "not set (defaults to LOOM_ADDR; set it for clone links on a LAN)".into()),
    }
    if let Ok(db) = db::Db::open(&data.join("loom.db")) {
        let users = db.users().map(|u| u.len()).unwrap_or(0);
        let repos = db.repo_count_total().unwrap_or(0);
        say(true, "database", format!("schema v{}, {users} users, {repos} repositories", db.schema_version()));
    } else {
        say(true, "database", "not created yet (first run makes it)".into());
    }
    if bad > 0 {
        return Err(format!("{bad} check(s) failed"));
    }
    println!("\nAll good. Start it with: loom serve");
    Ok(())
}

/// Restore a backup made by `loom backup`. Refuses to overwrite unless --force.
fn restore(src: Option<&str>, args: &[String]) -> Result<(), String> {
    let src = std::path::PathBuf::from(src.ok_or("usage: loom restore <backup.db> [--repos <dir>] [--force]")?);
    let data = std::path::PathBuf::from(std::env::var_os("LOOM_DATA").unwrap_or_else(|| "./data".into()));
    let force = args.iter().any(|a| a == "--force");
    let repos = opt(args, "--repos").map(std::path::PathBuf::from);
    if !src.is_file() {
        return Err(format!("{}: no such file", src.display()));
    }
    // Verify before touching anything.
    let check = db::Db::open(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    let users = check.users().map(|u| u.len()).unwrap_or(0);
    let repos_n = check.repo_count_total().unwrap_or(0);
    drop(check);
    println!("backup: {} users, {} repositories, schema v{}", users, repos_n, {
        let d = db::Db::open(&src).map_err(|e| e.to_string())?;
        d.schema_version()
    });
    std::fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let target = data.join("loom.db");
    if target.exists() && !force {
        return Err(format!("{} already exists; stop loom and pass --force to replace it", target.display()));
    }
    let tmp = data.join("loom.db.restore");
    std::fs::copy(&src, &tmp).map_err(|e| format!("copy: {e}"))?;
    std::fs::rename(&tmp, &target).map_err(|e| format!("move: {e}"))?;
    // WAL/SHM from the old database must not survive.
    let _ = std::fs::remove_file(data.join("loom.db-wal"));
    let _ = std::fs::remove_file(data.join("loom.db-shm"));
    println!("database -> {}", target.display());
    if let Some(r) = repos {
        if !r.is_dir() {
            return Err(format!("{}: no such directory", r.display()));
        }
        let dst = data.join("repos");
        // Replace, don't merge: git object directories are read-only, so
        // copying into an existing tree fails on mode.
        if dst.exists() {
            std::fs::remove_dir_all(&dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
        std::fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
        // Copy the *contents* into data/repos (cp -a dir dst would nest it).
        let mut src = r.clone();
        src.push(".");
        let out = std::process::Command::new("cp").args(["-a", "--reflink=auto"]).arg(&src).arg(&dst).output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        println!("repositories -> {}", dst.display());
    }
    println!("done. Start loom with LOOM_DATA={}", data.display());
    Ok(())
}
