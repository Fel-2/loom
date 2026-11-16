//! Embedded SSH server for git. Public-key auth against the DB; only
//! `git-upload-pack` / `git-receive-pack` are allowed — there is no shell.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{self, Auth, Msg, PublicKeyOffer, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};

use crate::app::App;
use crate::db::{Access, User};
use crate::git;

fn load_or_create_host_key(path: &Path) -> Result<PrivateKey, String> {
    if let Ok(pem) = std::fs::read_to_string(path) {
        return PrivateKey::from_openssh(pem).map_err(|e| format!("{}: {e}", path.display()));
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    let pem = key.to_openssh(LineEnding::LF).map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        std::io::Write::write_all(&mut f, pem.as_bytes()).map_err(|e| e.to_string())?;
    }
    eprintln!("generated SSH host key {}", path.display());
    Ok(key)
}

pub async fn serve(app: Arc<App>, addr: String) -> Result<(), String> {
    let key = load_or_create_host_key(&app.data.join("ssh_host_ed25519_key"))?;
    let config = server::Config {
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        auth_rejection_time: Duration::from_millis(500),
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![key],
        inactivity_timeout: Some(Duration::from_secs(600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        max_auth_attempts: 6,
        nodelay: true,
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| format!("ssh bind {addr}: {e}"))?;
    eprintln!("loom ssh on {addr}");
    Srv { app }.run_on_socket(Arc::new(config), &listener).await.map_err(|e| e.to_string())
}

struct Srv {
    app: Arc<App>,
}

impl server::Server for Srv {
    type Handler = Conn;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Conn {
        Conn { app: self.app.clone(), user: None, git_protocol: None, stdin: None }
    }
}

pub struct Conn {
    app: Arc<App>,
    user: Option<User>,
    git_protocol: Option<String>,
    stdin: Option<ChildStdin>,
}

impl Conn {
    fn lookup(&self, key: &russh::keys::PublicKey) -> Option<User> {
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        self.app.db.key_user(&fp).ok().flatten()
    }
}

/// Parse `git-upload-pack '/owner/name.git'` (or `git upload-pack ...`).
fn parse_command(cmd: &str) -> Option<(&'static str, String, String)> {
    let cmd = cmd.trim();
    let (svc, path) = match cmd.strip_prefix("git ") {
        Some(rest) => rest.split_once(' ')?,
        None => cmd.strip_prefix("git-")?.split_once(' ')?,
    };
    let svc = match svc {
        "upload-pack" => "upload-pack",
        "receive-pack" => "receive-pack",
        _ => return None,
    };
    let path = path.trim().trim_matches(|c| c == '\'' || c == '"').trim_matches('/');
    let (owner, name) = path.split_once('/')?;
    (crate::db::valid_name(owner) && !name.contains('/')).then(|| (svc, owner.to_owned(), name.to_owned()))
}

impl server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_publickey_offered(&mut self, _user: &str, offer: PublicKeyOffer<'_>) -> Result<Auth, Self::Error> {
        Ok(if self.lookup(offer.public_key).is_some() { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, _user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        self.user = self.lookup(key.public_key);
        Ok(if self.user.is_some() { Auth::Accept } else { Auth::reject() })
    }

    async fn authentication_banner(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(None)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let ok = name == "GIT_PROTOCOL"
            && value.len() < 256
            && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"=:._-".contains(&b));
        if ok {
            self.git_protocol = Some(value.to_owned());
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        let name = self.user.as_ref().map_or("", |u| u.name.as_str());
        let msg = format!("Hi {name}! loom does not provide shell access.\r\n");
        let handle = session.handle();
        session.channel_success(channel)?;
        tokio::spawn(async move {
            let _ = handle.extended_data(channel, 1, msg.into_bytes()).await;
            let _ = handle.exit_status_request(channel, 0).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
        Ok(())
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let handle = session.handle();
        let fail = |msg: String| {
            let handle = handle.clone();
            async move {
                let _ = handle.extended_data(channel, 1, format!("loom: {msg}\n").into_bytes()).await;
                let _ = handle.exit_status_request(channel, 1).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            }
        };
        let cmd = String::from_utf8_lossy(data).into_owned();
        let Some((svc, owner, name)) = parse_command(&cmd) else {
            session.channel_success(channel)?;
            tokio::spawn(fail("only git-upload-pack and git-receive-pack are supported".into()));
            return Ok(());
        };
        let need = if svc == "receive-pack" { Access::Write } else { Access::Read };
        let Some((repo, _)) = self.app.authorize(self.user.as_ref(), &owner, &name, need) else {
            session.channel_success(channel)?;
            tokio::spawn(fail(format!("repository '{owner}/{name}' not found or access denied")));
            return Ok(());
        };
        let dir = self.app.repo_dir(&repo);
        let mut cmd = Command::new("git");
        if svc == "receive-pack" {
            self.app.receive_pack_setup(&mut cmd, &repo, self.user.as_ref());
        } else {
            // loom's scratch refs (fork compares) are never served.
            cmd.args(["-c", "uploadpack.hideRefs=refs/loom"]);
        }
        cmd.arg(svc).arg(&dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(p) = &self.git_protocol {
            cmd.env("GIT_PROTOCOL", p);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ssh: spawn git: {e}");
                session.channel_success(channel)?;
                tokio::spawn(fail("internal error".into()));
                return Ok(());
            }
        };
        self.stdin = child.stdin.take();
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(russh::Error::Disconnect);
        };
        session.channel_success(channel)?;

        tokio::spawn(async move {
            let h2 = handle.clone();
            let err_task = async move {
                let mut buf = vec![0u8; 8 * 1024];
                while let Ok(n @ 1..) = stderr.read(&mut buf).await {
                    if h2.extended_data(channel, 1, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            };
            let h3 = handle.clone();
            let out_task = async move {
                let mut buf = vec![0u8; 32 * 1024];
                while let Ok(n @ 1..) = stdout.read(&mut buf).await {
                    if h3.data(channel, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            };
            tokio::join!(err_task, out_task);
            let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(1);
            if code == 0 && svc == "receive-pack" {
                let _ = tokio::task::spawn_blocking(move || git::fix_head(&dir)).await;
            }
            let _ = handle.exit_status_request(channel, code as u32).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
        Ok(())
    }

    async fn data(&mut self, _channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(stdin) = &mut self.stdin {
            if stdin.write_all(data).await.is_err() {
                self.stdin = None;
            }
        }
        Ok(())
    }

    async fn channel_eof(&mut self, _channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        // Closing git's stdin lets it finish.
        self.stdin = None;
        Ok(())
    }
}
