//! An app's secrets: values its node's operator gives it, which are never in
//! its package (a public OCI artifact, see `registry`).
//!
//! A node's `secrets` directory, which only its owner can use, holds each
//! app's as `<app>.json`, a JSON object of strings named as `wasi:config`
//! keys. The supervisor reads it when it starts the app's worker and hands
//! the values over on the worker's spec channel (its stdin), never in its
//! arguments or environment. The guest reads them with its config, through
//! `wasi:config/store`, over any of its package's config of the same name. A
//! change to the file restarts the worker (see `supervisor`).
//!
//! The host never copies a secret into what it reports: whatever it would
//! pass on of what the guest wrote (telemetry, the detail of a failed
//! request) has every secret value masked first.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, ensure};

use crate::registry::AppSpec;

/// An app's secrets by key. `Debug` shows only the keys.
#[derive(Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct Secrets(pub BTreeMap<String, String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl Secrets {
    /// Masks every secret value in `text` with as many `*`, so its length,
    /// and an OTLP batch's structure, stay as they were.
    pub fn mask(&self, text: &mut [u8]) {
        let view: &[u8] = text;
        let found: Vec<_> = self.0.values().filter(|v| !v.is_empty())
            .flat_map(|v| memchr::memmem::find_iter(view, v).map(|i| i..i + v.len()).collect::<Vec<_>>())
            .collect();
        for r in found {
            text[r].fill(b'*');
        }
    }

    /// `text`, masked. Whole characters become `*`, so it is still UTF-8.
    pub fn masked(&self, text: String) -> String {
        let mut bytes = text.into_bytes();
        self.mask(&mut bytes);
        String::from_utf8(bytes).expect("whole characters masked")
    }
}

/// The secrets `file` holds (none if there is no such file), which must
/// give every key in `needed`. What goes wrong is said without quoting it.
pub fn load(file: Option<&Path>, needed: &BTreeSet<String>) -> anyhow::Result<Secrets> {
    let name = file.and_then(Path::file_name).map_or("no file".into(), |n| n.to_string_lossy().into_owned());
    let secrets = match file.map(|f| (f, std::fs::metadata(f))) {
        None => Secrets::default(),
        Some((_, Err(e))) if e.kind() == std::io::ErrorKind::NotFound => Secrets::default(),
        Some((f, m)) => {
            ensure!(m.with_context(|| name.clone())?.is_file(), "{name} is not a file");
            let bytes = std::fs::read(f).with_context(|| name.clone())?;
            // A parse error can quote what it read (`invalid type: string "…"`).
            serde_json::from_slice(&bytes).map_err(|e| {
                anyhow::anyhow!("{name} is not a JSON object of strings (line {}, column {})", e.line(), e.column())
            })?
        }
    };
    let missing: Vec<_> = needed.iter().filter(|k| !secrets.0.contains_key(*k)).map(String::as_str).collect();
    let given = if file.is_some() { format!("{name} doesn't give them") } else { "none were given".into() };
    ensure!(missing.is_empty(), "the app needs secrets {}; {given}", missing.join(", "));
    Ok(secrets)
}

/// What changes when `file` does: it is written, replaced, made or removed.
pub type Stamp = Option<(u64, u64, i64, i64)>;

pub fn stamp(file: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(file).ok().map(|m| (m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

/// A node's secrets directory is one that only its owner, this user, can use.
pub fn check_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dir).with_context(|| format!("secrets directory {}", dir.display()))?;
    let private = m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0;
    ensure!(private, "secrets directory {}: only its owner, the user nanovm runs as, may use it (chmod 700)", dir.display());
    Ok(())
}

/// Refuses a manifest that would publish a secret: a `config` or `env`
/// entry for a key it declares secret, or one whose key or value looks like
/// a credential's. It names the key, never the value.
pub fn check_package(spec: &AppSpec) -> anyhow::Result<()> {
    for (table, entries) in [("config", &spec.config), ("env", &spec.env)] {
        for (k, v) in entries {
            ensure!(!spec.secrets.contains(k), "{table}.{k} is one of the app's `secrets`, which each node's operator gives it");
            let credential = credential_name(k) || credential_value(v);
            ensure!(v.is_empty() || !credential, "{table}.{k} looks like a credential, and a package is public: \
                list {k:?} in the manifest's `secrets` and give it to nodes (README: Secrets)");
        }
    }
    Ok(())
}

/// `client_secret`, `dbPassword`, `BOT_TOKEN`, `api_key`; not `token_url`
/// or `max_tokens`.
fn credential_name(key: &str) -> bool {
    let key: String = key.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    ["secret", "password", "passwd", "passphrase", "token", "credential", "credentials", "apikey", "privatekey", "accesskey", "secretkey"]
        .iter().any(|w| key.ends_with(w))
}

/// A private key, a URL with a password, or a token with a well-known
/// provider's prefix (OpenAI and Anthropic, Stripe, GitHub, GitLab, Slack,
/// AWS, Google, Hugging Face).
fn credential_value(v: &str) -> bool {
    const PREFIXES: [&str; 15] = ["sk-", "sk_live_", "rk_live_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_",
        "glpat-", "xoxb-", "xoxp-", "AKIA", "AIza", "hf_"];
    let token = |p: &&str| v.strip_prefix(*p).is_some_and(|t| t.len() >= 16 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)));
    v.contains("PRIVATE KEY-----") || reqwest::Url::parse(v).is_ok_and(|u| u.password().is_some()) || PREFIXES.iter().any(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(test: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nanovm-secrets-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn needs(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    /// A file gives its keys; no file gives none; a malformed or unreadable
    /// one, or one without a needed key, is refused, and the refusal never
    /// quotes what the file holds.
    #[test]
    fn load_reads_a_file_and_refuses_without_quoting_it() {
        let d = dir("load");
        let f = d.join("app.json");
        std::fs::write(&f, r#"{"api_key": "k-123", "other": "o"}"#).unwrap();
        let s = load(Some(&f), &needs(&["api_key"])).unwrap();
        assert_eq!(s.0["api_key"], "k-123");
        assert_eq!(format!("{s:?}"), r#"{"api_key", "other"}"#, "Debug shows keys only");
        assert_eq!(load(Some(&d.join("none.json")), &needs(&[])).unwrap(), Secrets::default());
        assert_eq!(load(None, &needs(&[])).unwrap(), Secrets::default());

        let e = load(None, &needs(&["api_key", "b"])).unwrap_err().to_string();
        assert_eq!(e, "the app needs secrets api_key, b; none were given");
        let e = load(Some(&f), &needs(&["api_key", "missing"])).unwrap_err().to_string();
        assert_eq!(e, "the app needs secrets missing; app.json doesn't give them");
        // serde_json would say `invalid type: string "sk-…"` or `integer `…``.
        for bad in [r#""sk-0123456789abcdefSECRET""#, r#"{"k": 4242424242424242}"#, r#"{"k": "s3cret-value""#, "{\"k\": [\"s3cret-value\"]}"] {
            std::fs::write(&f, bad).unwrap();
            let e = format!("{:#}", load(Some(&f), &needs(&[])).unwrap_err());
            assert!(e.starts_with("app.json is not a JSON object of strings (line 1, column"), "{e}");
            for value in ["SECRET", "4242", "s3cret"] {
                assert!(!e.contains(value), "the error quotes the file: {e}");
            }
        }
        std::fs::remove_file(&f).unwrap();
        std::fs::create_dir(&f).unwrap();
        assert_eq!(load(Some(&f), &needs(&[])).unwrap_err().to_string(), "app.json is not a file");
        std::fs::remove_dir_all(d).unwrap();
    }

    /// Every occurrence of every value is masked, overlapping ones too, and
    /// text stays its length and UTF-8.
    #[test]
    fn mask_hides_every_value_in_place() {
        let s = Secrets([("a", "s3cret-é"), ("b", "et-é!x"), ("empty", "")].map(|(k, v)| (k.into(), v.into())).into());
        let text = "x s3cret-é!x and s3cret-é, é".to_string();
        let masked = s.masked(text.clone());
        assert_eq!(masked.len(), text.len());
        assert_eq!(masked, format!("x {} and {}, é", "*".repeat(11), "*".repeat(9)));
    }

    /// The directory must be the owner's alone.
    #[test]
    fn the_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("private");
        for (mode, ok) in [(0o700, true), (0o500, true), (0o750, false), (0o701, false)] {
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(check_dir(&d).is_ok(), ok, "{mode:o}");
        }
        assert!(check_dir(&d.join("missing")).is_err());
        std::fs::write(d.join("file"), "").unwrap();
        std::fs::set_permissions(d.join("file"), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_dir(&d.join("file")).is_err(), "not a directory");
        std::fs::remove_dir_all(d).unwrap();
    }

    /// A package can't carry a declared secret, or a value named or shaped
    /// like a credential; ordinary config passes, and errors don't quote values.
    #[test]
    fn packages_refuse_credentials() {
        let spec = |manifest: serde_json::Value| check_package(&serde_json::from_value(manifest).unwrap()).map_err(|e| e.to_string());
        let ok = serde_json::json!({
            "config": {"token_url": "https://github.com/login/oauth/access_token", "max_tokens": "200", "client_id": "Iv1.abc",
                       "client_secret": "", "upstream": "https://user@llm.example.com/v1", "locale": "sk-SK"},
            "env": {"RUST_LOG": "info"},
            "secrets": ["upstream_key"],
        });
        spec(ok).unwrap();
        for (manifest, says) in [
            (serde_json::json!({"secrets": ["k"], "config": {"k": ""}}), "config.k is one of the app's `secrets`"),
            (serde_json::json!({"secrets": ["K"], "env": {"K": "v"}}), "env.K is one of the app's `secrets`"),
            (serde_json::json!({"config": {"client_secret": "0123abcd"}}), "config.client_secret looks like a credential"),
            (serde_json::json!({"env": {"BOT_TOKEN": "0123abcd"}}), "env.BOT_TOKEN looks like a credential"),
            (serde_json::json!({"config": {"dbPassword": "0123abcd"}}), "config.dbPassword looks like a credential"),
            (serde_json::json!({"config": {"api-key": "0123abcd"}}), "config.api-key looks like a credential"),
            (serde_json::json!({"env": {"UPSTREAM": "sk-ant-0123456789abcdefghij"}}), "env.UPSTREAM looks like a credential"),
            (serde_json::json!({"config": {"gh": "ghp_0123456789abcdefghij0123"}}), "config.gh looks like a credential"),
            (serde_json::json!({"config": {"db": "postgres://app:0123abcd@db/app"}}), "config.db looks like a credential"),
            (serde_json::json!({"config": {"pem": "-----BEGIN PRIVATE KEY-----\n0123abcd"}}), "config.pem looks like a credential"),
        ] {
            let e = spec(manifest).unwrap_err();
            assert!(e.starts_with(says) && !e.contains("0123"), "{e}");
        }
    }
}
