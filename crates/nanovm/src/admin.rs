//! The operator's one control path into a running node (`nanovm serve`): a
//! Unix socket at `<data_dir>/admin/socket`, in a directory only the
//! supervisor's own user may enter (root, in production). Workers run as
//! other uids, and their jail lets them make no socket, so neither a guest
//! nor its worker reaches it; nor does any HTTP route.
//!
//! One command per connection: the client sends its arguments as one line
//! of JSON, an array of strings, and gets one line back, `{"ok": …}` or
//! `{"error": "…"}`. `Supervisor::command` runs them.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Weak;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use crate::config::NodeConfig;
use crate::supervisor::Supervisor;

/// The longest command accepted.
const MAX_LINE: u64 = 64 << 10;

/// Where a node's admin socket is.
pub fn socket(cfg: &NodeConfig) -> PathBuf {
    cfg.data_dir.join("admin").join("socket")
}

/// Binds the admin socket, in a directory of mode 0700 that must be the
/// supervisor's own.
pub fn listen(cfg: &NodeConfig) -> anyhow::Result<tokio::net::UnixListener> {
    let path = socket(cfg);
    let dir = path.parent().expect("in admin/");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let md = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions.
    anyhow::ensure!(md.is_dir() && md.uid() == unsafe { libc::geteuid() }, "{} is not the supervisor's own directory", dir.display());
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.into());
    }
    tokio::net::UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))
}

/// Serves commands on `listener` while the supervisor lives.
pub async fn serve(listener: tokio::net::UnixListener, sup: Weak<Supervisor>) {
    loop {
        let conn = match listener.accept().await {
            Ok((conn, _)) => conn,
            Err(e) => {
                eprintln!("nanovm: admin socket: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(sup) = sup.upgrade() else { return };
        tokio::spawn(async move {
            let (read, mut write) = conn.into_split();
            let mut line = Vec::new();
            let _ = tokio::io::BufReader::new(read).take(MAX_LINE).read_until(b'\n', &mut line).await;
            let reply = match serde_json::from_slice::<Vec<String>>(&line) {
                Ok(args) => match sup.command(&args).await {
                    Ok(v) => serde_json::json!({ "ok": v }),
                    Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
                },
                Err(e) => serde_json::json!({ "error": format!("a command is a JSON array of strings: {e}") }),
            };
            let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
        });
    }
}

/// `nanovm admin`: sends `args` to the node `cfg` describes and prints its
/// answer; an error answer is an error.
pub fn client(cfg: &NodeConfig, args: &[String]) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let path = socket(cfg);
    let mut conn = std::os::unix::net::UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;
    conn.write_all(format!("{}\n", serde_json::to_string(args)?).as_bytes())?;
    let mut reply = String::new();
    conn.read_to_string(&mut reply)?;
    let reply: serde_json::Value = serde_json::from_str(&reply).with_context(|| format!("the node answered {reply:?}"))?;
    if let Some(e) = reply.get("error") {
        anyhow::bail!("{}", e.as_str().unwrap_or_default());
    }
    println!("{}", serde_json::to_string_pretty(&reply["ok"])?);
    Ok(())
}
