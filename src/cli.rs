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
       loom key add <user> <pubkey-file|-> [title]
       loom repo create <owner>/<name> [--private] [--desc <text>]
       loom repo adopt        register bare repos already on disk (public)
       loom repo list

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
        ("key", Some("add")) => key_add(&app, rest),
        ("repo", Some("create")) => repo_create(&app, rest),
        ("repo", Some("adopt")) => repo_adopt(&app),
        ("repo", Some("list")) => repo_list(&app),
        _ => Err(USAGE.into()),
    })
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

fn repo_list(app: &App) -> Result<(), String> {
    let admin = db::User { id: 0, name: String::new(), email: String::new(), is_admin: true };
    for r in app.db.visible_repos(Some(&admin), None).map_err(|e| e.to_string())? {
        println!("{}/{}{}", r.owner, r.name, if r.private { "  private" } else { "" });
    }
    Ok(())
}
