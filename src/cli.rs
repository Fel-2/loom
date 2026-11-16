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
