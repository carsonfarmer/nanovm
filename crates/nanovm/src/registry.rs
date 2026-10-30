//! Apps from an OCI registry, named per request by the proxy (see
//! [`AppRef::from_headers`]): pulled anonymously by digest, verified against
//! the request's trust policy with Sigstore (offline, against the embedded
//! public-good trusted root), and cached by digest.
//!
//! An app is an OCI artifact of type [`APP_TYPE`]: its config blob is the
//! app's [`AppSpec`] and its one layer the component. Its version is the
//! manifest's `org.opencontainers.image.version`. Signatures are Sigstore
//! bundles attached as referrers (GitHub artifact attestations are), signed
//! over the manifest digest, which in turn pins every blob.
//!
//! An app's data belongs to its repository: every version of it, whoever
//! signed it, sees the same volumes. Only those who can push to a repository
//! can put a signed artifact in it, so naming another identity's app can
//! never reach a different app's data.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use http::HeaderMap;
use oci_spec::distribution::Reference;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sigstore_trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, TrustedRoot};
use sigstore_verify::{VerificationPolicy, Verifier};

use crate::config::Limits;
use crate::egress::is_global;

pub const APP_TYPE: &str = "application/vnd.nanovm.app.v1+json";
pub const CONFIG_TYPE: &str = "application/vnd.nanovm.config.v1+json";
pub const WASM_TYPE: &str = "application/wasm";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const BUNDLE_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";

/// The fields of an OCI manifest the node reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    artifact_type: Option<String>,
    config: Descriptor,
    layers: Vec<Descriptor>,
    #[serde(default)]
    annotations: HashMap<String, String>,
}

/// The fields of an OCI index (here, a referrers list) the node reads.
#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
    artifact_type: Option<String>,
}
pub const VERSION: &str = "org.opencontainers.image.version";
/// The most a manifest, index, config, token or signature may be.
const MAX_METADATA: u64 = 4 << 20;
/// Signatures looked at per load; past these the app counts as unsigned.
const MAX_SIGNATURES: usize = 16;

/// The headers the proxy names an app with. The node trusts them because
/// only the proxy reaches it; the proxy drops any a client sends.
pub const APP_HEADER: &str = "nanovm-app";
pub const TRUST_HEADER: &str = "nanovm-trust";
pub const MIN_VERSION_HEADER: &str = "nanovm-min-version";
/// The host the proxy found the app at: an app is a host's deployment of a
/// repository, so a host naming another's repository gets data of its own.
pub const HOST_HEADER: &str = "nanovm-host";

/// Which app version a request wants, and whose signature it accepts.
#[derive(Debug, Clone)]
pub struct AppRef {
    /// Where the app is served; empty for a node driven directly (tests).
    pub host: String,
    /// `registry/repository@sha256:…`
    pub reference: Reference,
    pub digest: String,
    pub policy: Policy,
}

/// Whom an app version must be signed by, and its version floor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Policy {
    /// `(issuer, subject)` pairs; a signature by any of them is accepted.
    pub trust: Vec<(String, String)>,
    pub min_version: Option<semver::Version>,
}

impl AppRef {
    /// `None` when the request names no app.
    pub fn from_headers(h: &HeaderMap) -> anyhow::Result<Option<AppRef>> {
        let get = |name| h.get(name).map(|v| v.to_str().with_context(|| format!("{name} is not text")));
        let Some(app) = get(APP_HEADER) else { return Ok(None) };
        let reference: Reference = app?.parse().context(APP_HEADER)?;
        let digest = reference.digest().context("nanovm-app must name a digest")?.to_string();
        check_digest(&digest)?;
        let trust = get(TRUST_HEADER)
            .context("nanovm-trust is required")??
            .split(',')
            .map(|t| match t.split_whitespace().collect::<Vec<_>>()[..] {
                [issuer, subject] => Ok((issuer.to_string(), subject.to_string())),
                _ => bail!("nanovm-trust entries are `<issuer> <subject>`"),
            })
            .collect::<anyhow::Result<_>>()?;
        let min_version = get(MIN_VERSION_HEADER).transpose()?.map(str::parse).transpose()?;
        Ok(Some(AppRef {
            host: get(HOST_HEADER).transpose()?.unwrap_or_default().to_ascii_lowercase(),
            reference,
            digest,
            policy: Policy { trust, min_version },
        }))
    }

    /// The app's name on this node: its host and repository, readable and
    /// unique.
    pub fn name(&self) -> String {
        app_name(&self.host, &self.reference)
    }
}

/// `host registry/repository` as an app name: the readable part, then a
/// hash of the whole, so that no two (host, repository) pairs share a name.
/// A registry is named one way only: lower case, without `:443`. Whoever
/// controls a host chooses which version of a repository it runs, so every
/// host gets the repository's app (its volumes, uid and worker) to itself.
pub fn app_name(host: &str, r: &Reference) -> String {
    let registry = r.registry().to_ascii_lowercase();
    let registry = registry.strip_suffix(":443").unwrap_or(&registry);
    let repo = format!("{host} {registry}/{}", r.repository()).trim_start().to_string();
    let safe = |c: char| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') { c } else { '_' };
    let readable: String = repo.chars().map(safe).take(100).collect();
    format!("{readable}-{}", &format!("{:x}", Sha256::digest(&repo))[..16])
}

/// A digest is `sha256:` and 64 lowercase hex digits; anything else could
/// name a path.
fn check_digest(d: &str) -> anyhow::Result<()> {
    let hex = d.strip_prefix("sha256:").unwrap_or_default();
    ensure!(
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "bad digest {d:?}"
    );
    Ok(())
}

/// An app's config blob.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

pub struct Pulled {
    pub spec: AppSpec,
    pub wasm: Vec<u8>,
    pub version: semver::Version,
}

pub struct Registry {
    oci: Oci,
    verifier: Verifier,
    /// Content-addressed: `<dir>/<digest>` holds a verified blob.
    dir: PathBuf,
    max_component: u64,
}

impl Registry {
    /// With `dev`, signatures go unchecked (see `NodeConfig::dev`).
    pub fn new(dir: PathBuf, max_component: u64, dev: bool) -> anyhow::Result<Registry> {
        let root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)?;
        std::fs::create_dir_all(&dir)?;
        Ok(Registry {
            oci: Oci::new(dev)?,
            verifier: Verifier::new(&root)?,
            dir,
            max_component,
        })
    }

    /// The app `r` names, if it is authorized (see `authorize`). Nothing but
    /// the manifest and the signatures is downloaded before that is settled.
    pub async fn load(&self, r: &AppRef) -> anyhow::Result<Pulled> {
        // Not cached: nothing reaches the disk before the app is authorized.
        let bytes = self.fetch(r, "manifests", &r.digest, MANIFEST_TYPE, MAX_METADATA, false).await?;
        let manifest: Manifest = serde_json::from_slice(&bytes).context("the manifest")?;
        let version = manifest
            .annotations
            .get(VERSION)
            .context("the manifest has no org.opencontainers.image.version")?;
        let version: semver::Version = version.parse().with_context(|| format!("version {version:?}"))?;
        self.authorize(r, &version).await?;
        ensure!(manifest.artifact_type.as_deref() == Some(APP_TYPE), "not a nanovm app ({APP_TYPE})");
        ensure!(manifest.config.media_type == CONFIG_TYPE, "config is not {CONFIG_TYPE}");
        let [layer] = &manifest.layers[..] else { bail!("an app has exactly one layer") };
        ensure!(layer.media_type == WASM_TYPE, "the layer is not {WASM_TYPE}");
        let spec = self.blob(r, &manifest.config, MAX_METADATA).await?;
        Ok(Pulled {
            spec: serde_json::from_slice(&spec).context("the app's config")?,
            wasm: self.blob(r, layer, self.max_component).await.context("the component")?,
            version,
        })
    }

    /// Whether `r`'s policy accepts the app at `version`: signed by one of
    /// its identities, and at least its floor.
    pub async fn authorize(&self, r: &AppRef, version: &semver::Version) -> anyhow::Result<()> {
        if !self.oci.dev {
            self.verify(r).await?;
        }
        if let Some(min) = &r.policy.min_version {
            ensure!(version >= min, "version {version} is below the minimum {min}");
        }
        Ok(())
    }

    async fn blob(&self, r: &AppRef, d: &Descriptor, max: u64) -> anyhow::Result<Vec<u8>> {
        ensure!(d.size <= max, "{} is {} bytes, over {max}", d.digest, d.size);
        let bytes = self.fetch(r, "blobs", &d.digest, "*/*", d.size, true).await?;
        ensure!(bytes.len() as u64 == d.size, "{} is not {} bytes", d.digest, d.size);
        Ok(bytes)
    }

    /// The bytes `digest` names, at most `max` of them, checked against it;
    /// from the cache if `cache` and it has them, else fetched (and kept).
    async fn fetch(&self, r: &AppRef, kind: &str, digest: &str, accept: &str, max: u64, cache: bool) -> anyhow::Result<Vec<u8>> {
        check_digest(digest)?;
        let path = self.dir.join(digest.replace(':', "-"));
        if cache && let Ok(Ok(b)) = tokio::task::spawn_blocking({
            let path = path.clone();
            move || std::fs::read(path)
        })
        .await
        {
            return Ok(b);
        }
        let bytes = self.oci.get(&r.reference, kind, digest, accept, max).await?.with_context(|| format!("{digest} not found"))?;
        let actual = format!("sha256:{:x}", Sha256::digest(&bytes));
        ensure!(actual == digest, "the registry sent {actual} for {digest}");
        if !cache {
            return Ok(bytes);
        }
        // Written aside and renamed, so a reader only sees a complete file.
        tokio::task::spawn_blocking(move || {
            static WRITER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = WRITER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = path.with_extension(format!("tmp.{n}"));
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
            Ok(bytes)
        })
        .await?
    }

    /// Checks the attached signatures against the trust policy. They are
    /// fetched every time: whether a signature exists is the registry's
    /// current answer, and they are small.
    async fn verify(&self, r: &AppRef) -> anyhow::Result<()> {
        let hex = r.digest.trim_start_matches("sha256:");
        let index = match self.oci.get(&r.reference, "referrers", &r.digest, INDEX_TYPE, MAX_METADATA).await? {
            Some(b) => Some(b),
            // A registry without the referrers API keeps them under a tag.
            None => self.oci.get(&r.reference, "manifests", &format!("sha256-{hex}"), INDEX_TYPE, MAX_METADATA).await?,
        };
        let index: Index = match index {
            Some(b) => serde_json::from_slice(&b).context("the referrers")?,
            None => bail!("not signed by a trusted identity (no signatures)"),
        };
        let hash = sigstore_types::Sha256Hash::from_hex(hex)?;
        let mut why = String::from("no signature");
        let signatures = index.manifests.iter().filter(|m| m.artifact_type.as_deref() == Some(BUNDLE_TYPE));
        let mut left = MAX_SIGNATURES;
        for entry in signatures.take(MAX_SIGNATURES) {
            let m = self.fetch(r, "manifests", &entry.digest, MANIFEST_TYPE, MAX_METADATA, false).await?;
            let m: Manifest = serde_json::from_slice(&m)?;
            for layer in m.layers.iter().filter(|l| l.media_type == BUNDLE_TYPE) {
                left = left.checked_sub(1).context("too many signatures")?;
                ensure!(layer.size <= MAX_METADATA, "a signature is too large");
                let json = self.fetch(r, "blobs", &layer.digest, "*/*", layer.size, false).await?;
                let bundle = sigstore_types::Bundle::from_json(std::str::from_utf8(&json)?)?;
                for (issuer, subject) in &r.policy.trust {
                    let policy = VerificationPolicy::new(subject.as_str(), issuer.as_str());
                    match self.verifier.verify(hash, &bundle, &policy) {
                        Ok(_) => return Ok(()),
                        Err(e) => why = e.to_string(),
                    }
                }
            }
        }
        bail!("not signed by a trusted identity ({why})")
    }
}

/// Anonymous reads from OCI registries, each bounded in size. Registries are
/// named by whoever deploys an app, so none of them may be on a private
/// address, unless `dev`: then loopback ones are spoken to over plain HTTP.
pub struct Oci {
    http: reqwest::Client,
    dev: bool,
}

impl Oci {
    pub fn new(dev: bool) -> anyhow::Result<Oci> {
        let redirect = reqwest::redirect::Policy::custom(move |a| match a.url().host_str() {
            _ if a.previous().len() > 5 => a.error("too many redirects"),
            Some(h) if allowed(h, dev) => a.follow(),
            _ => a.error("a redirect to a private address"),
        });
        let http = reqwest::Client::builder()
            // A forward proxy would resolve names itself, past `PublicOnly`.
            .no_proxy()
            .dns_resolver(Arc::new(PublicOnly(dev)))
            .redirect(redirect)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Oci { http, dev })
    }

    /// `GET /v2/<repository>/<kind>/<what>`, at most `max` bytes of it;
    /// `None` if the registry has no such thing.
    pub async fn get(&self, r: &Reference, kind: &str, what: &str, accept: &str, max: u64) -> anyhow::Result<Option<Vec<u8>>> {
        let res = self.send(r, kind, what, accept).await?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(read(res.error_for_status()?, max).await?))
    }

    /// The digest of the manifest `r`'s tag names now: its hash, so no
    /// header has to be taken on trust.
    pub async fn digest(&self, r: &Reference) -> anyhow::Result<String> {
        let tag = r.tag().context("no tag")?;
        let manifest = self.get(r, "manifests", tag, MANIFEST_TYPE, MAX_METADATA).await?;
        Ok(format!("sha256:{:x}", Sha256::digest(manifest.context("no such tag")?)))
    }

    async fn send(&self, r: &Reference, kind: &str, what: &str, accept: &str) -> anyhow::Result<reqwest::Response> {
        let host = r.resolve_registry();
        let mut url = reqwest::Url::parse(&format!("https://{host}/v2/{}/{kind}/{what}", r.repository()))?;
        // Checked as parsed, which is how it will be connected to: `127.1`
        // is 127.0.0.1 by then.
        ensure!(url.host_str().is_some_and(|h| allowed(h, self.dev)), "registry {host} is on a private address");
        if self.dev && url.host_str().is_some_and(loopback) {
            url.set_scheme("http").expect("http is a valid scheme");
        }
        let req = || self.http.get(url.clone()).header(http::header::ACCEPT, accept);
        let res = req().send().await?;
        if res.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(res);
        }
        let challenge = res.headers().get(http::header::WWW_AUTHENTICATE).and_then(|v| v.to_str().ok());
        let token = self.token(challenge.context("401 without a challenge")?).await?;
        Ok(req().bearer_auth(token).send().await?)
    }

    /// An anonymous token for a `Bearer realm="…",service="…",scope="…"`
    /// challenge.
    async fn token(&self, challenge: &str) -> anyhow::Result<String> {
        let params = challenge.strip_prefix("Bearer ").context("not a bearer challenge")?;
        let params: HashMap<_, _> = params
            .split("\",")
            .filter_map(|kv| kv.trim().split_once('='))
            .map(|(k, v)| (k, v.trim_matches('"')))
            .collect();
        let realm = reqwest::Url::parse(params.get("realm").context("no realm")?)?;
        ensure!(realm.host_str().is_some_and(|h| allowed(h, self.dev)), "token realm on a private address");
        let query = ["service", "scope"].into_iter().filter_map(|k| Some((k, *params.get(k)?)));
        let res = self.http.get(realm).query(&query.collect::<Vec<_>>()).send().await?.error_for_status()?;
        let v: serde_json::Value = serde_json::from_slice(&read(res, MAX_METADATA).await?)?;
        let token = v.get("token").or_else(|| v.get("access_token")).and_then(|t| t.as_str());
        Ok(token.context("no token")?.to_string())
    }
}

/// A response's body, if it is at most `max` bytes.
async fn read(mut res: reqwest::Response, max: u64) -> anyhow::Result<Vec<u8>> {
    ensure!(res.content_length().is_none_or(|n| n <= max), "{} is over {max} bytes", res.url());
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        ensure!((out.len() + chunk.len()) as u64 <= max, "{} is over {max} bytes", res.url());
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether a registry host, as a parsed URL has it, may be spoken to: names
/// are checked once resolved (`PublicOnly`), addresses here.
fn allowed(host: &str, dev: bool) -> bool {
    match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        Ok(ip) => dev || is_global(ip),
        Err(_) => true,
    }
}

/// Resolves names, refusing any with a private address unless in dev mode.
struct PublicOnly(bool);

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let (dev, name) = (self.0, name.as_str().to_string());
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if !dev && !addrs.iter().all(|a| is_global(a.ip())) {
                return Err(format!("{name} has a private address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (http::HeaderName::from_static(k), v.parse().unwrap()))
            .collect()
    }

    fn app() -> String {
        format!("ghcr.io/acme/app@{DIGEST}")
    }

    fn err(h: &HeaderMap) -> String {
        format!("{:#}", AppRef::from_headers(h).unwrap_err())
    }

    /// A request without `nanovm-app` names no app, whatever else it
    /// carries.
    #[test]
    fn no_app_header_is_no_app() {
        assert!(AppRef::from_headers(&HeaderMap::new()).unwrap().is_none());
        let h = headers(&[(TRUST_HEADER, "garbage"), (MIN_VERSION_HEADER, "x")]);
        assert!(AppRef::from_headers(&h).unwrap().is_none());
    }

    /// The app is pinned by a well-formed digest: a tag alone, an
    /// unparsable reference or a value that isn't text is refused.
    #[test]
    fn app_must_be_a_reference_with_a_digest() {
        let trust = (TRUST_HEADER, "https://issuer subject");
        assert!(err(&headers(&[(APP_HEADER, "ghcr.io/acme/app:1.0"), trust])).contains("must name a digest"));
        assert!(err(&headers(&[(APP_HEADER, "ghcr.io/acme/app"), trust])).contains("must name a digest"));
        assert!(err(&headers(&[(APP_HEADER, "Not A Reference!"), trust])).contains(APP_HEADER));
        let mut h = headers(&[trust]);
        h.insert(APP_HEADER, http::HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(err(&h).contains("not text"));

        let tagged = format!("ghcr.io/acme/app:1.0@{DIGEST}");
        let r = AppRef::from_headers(&headers(&[(APP_HEADER, &tagged), trust]))
            .unwrap()
            .unwrap();
        assert_eq!(r.digest, DIGEST);
        assert_eq!(r.reference.repository(), "acme/app");
    }

    /// Only `sha256:` and 64 lowercase hex digits is a digest, so none can
    /// name a path.
    #[test]
    fn digests_are_checked() {
        check_digest(DIGEST).unwrap();
        for bad in [
            "",
            "sha256:",
            &DIGEST[..70],
            &DIGEST.to_uppercase(),
            &DIGEST.replace("sha256", "sha512"),
            &format!("{DIGEST}0"),
            &format!("sha256:../../{}", &DIGEST[13..]),
        ] {
            assert!(check_digest(bad).is_err(), "{bad:?}");
        }
    }

    /// A registry is checked as it will be connected to: non-canonical
    /// IPv4 forms are private addresses too, and names are left to the
    /// resolver.
    #[tokio::test]
    async fn registries_on_private_addresses_are_refused() {
        let oci = Oci::new(false).unwrap();
        for host in ["127.1", "2130706433:443", "0x7f.1", "10.1:5000", "169.254.169.254"] {
            let r: Reference = format!("{host}/x/y:t").parse().unwrap();
            let e = oci.get(&r, "manifests", "t", MANIFEST_TYPE, 1).await.unwrap_err().to_string();
            assert!(e.contains("private address"), "{host}: {e}");
        }
    }

    /// An app's name is its host, registry and repository, readable, safe
    /// as a path component, and distinct for distinct hosts and
    /// repositories, whatever the tag or digest or how the registry is
    /// written.
    #[test]
    fn apps_are_named_by_host_and_repository() {
        let name = |s: &str| app_name("", &s.parse().unwrap());
        let a = name("ghcr.io/acme/app:1");
        assert_eq!(a, name("GHCR.IO/acme/app:1"));
        assert_eq!(a, name("ghcr.io:443/acme/app:1"));
        let hosted = app_name("example.com", &"ghcr.io/acme/app:1".parse().unwrap());
        assert!(hosted.starts_with("example.com_ghcr.io_acme_app-"), "{hosted}");
        assert_ne!(hosted, app_name("evil.com", &"ghcr.io/acme/app:1".parse().unwrap()));
        assert_ne!(hosted, a);
        assert!(a.starts_with("ghcr.io_acme_app-") && a.len() == "ghcr.io_acme_app-".len() + 16, "{a}");
        assert_eq!(a, name(&format!("ghcr.io/acme/app@{DIGEST}")));
        for other in ["ghcr.io/acme/app2:1", "ghcr.io/acme_app:1", "localhost:5000/acme/app:1"] {
            assert_ne!(a, name(other), "{other}");
        }
        let long = name(&format!("ghcr.io/{}:1", "a/".repeat(60) + "b"));
        assert!(crate::table::valid_name(&long) && crate::table::valid_name(&name("localhost:5000/x:y")), "{long}");
    }

    /// Trust is a comma-separated list of `<issuer> <subject>`, with any
    /// whitespace around and between them; every entry must be a pair.
    #[test]
    fn trust_is_a_list_of_issuer_subject_pairs() {
        let app = app();
        let h = headers(&[(APP_HEADER, &app), (TRUST_HEADER, " i1  s1 ,i2\ts2,  i3 s3  ")]);
        let r = AppRef::from_headers(&h).unwrap().unwrap();
        let pairs = [("i1", "s1"), ("i2", "s2"), ("i3", "s3")].map(|(i, s)| (i.to_string(), s.to_string()));
        assert_eq!(r.policy.trust, pairs);
        assert_eq!(r.policy.min_version, None);

        for bad in ["", "i1", "i1 s1 extra", "i1 s1,", "i1 s1,,i2 s2", ","] {
            let h = headers(&[(APP_HEADER, &app), (TRUST_HEADER, bad)]);
            assert!(err(&h).contains("<issuer> <subject>"), "{bad:?}");
        }
        assert!(err(&headers(&[(APP_HEADER, &app)])).contains("nanovm-trust is required"));
    }

    /// The version floor is optional and must be full semver.
    #[test]
    fn min_version_is_semver() {
        let app = app();
        let with = |v| headers(&[(APP_HEADER, &app), (TRUST_HEADER, "i s"), (MIN_VERSION_HEADER, v)]);
        let r = AppRef::from_headers(&with("1.2.3-rc.1")).unwrap().unwrap();
        assert_eq!(r.policy.min_version, Some(semver::Version::parse("1.2.3-rc.1").unwrap()));
        for bad in ["1.2", "v1.2.3", "", "latest"] {
            assert!(AppRef::from_headers(&with(bad)).is_err(), "{bad:?}");
        }
    }

    /// An app's config blob takes only the known keys; each is optional.
    #[test]
    fn app_spec_defaults_and_refuses_unknown_keys() {
        let spec: AppSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.limits.memory_mb, Limits::default().memory_mb);
        assert!(spec.env.is_empty() && spec.config.is_empty());
        let spec: AppSpec = serde_json::from_str(r#"{"limits":{"cpu_ms":7},"env":{"A":"b"}}"#).unwrap();
        assert_eq!((spec.limits.cpu_ms, spec.limits.memory_mb), (7, 64));
        assert!(serde_json::from_str::<AppSpec>(r#"{"limit":{}}"#).is_err());
        assert!(serde_json::from_str::<AppSpec>(r#"{"limits":{"cpu":1}}"#).is_err());
    }

    /// Outside dev mode no registry address may be private (names are the
    /// resolver's to check); in dev mode any may, and loopback ones are
    /// plain HTTP.
    #[test]
    fn registries_are_public_unless_dev() {
        for private in ["127.0.0.1", "[::1]", "10.0.0.1", "169.254.169.254"] {
            assert!(!allowed(private, false), "{private}");
            assert!(allowed(private, true), "{private}");
        }
        for public in ["ghcr.io", "8.8.8.8", "[2001:4860::1]"] {
            assert!(allowed(public, false), "{public}");
        }
        assert!(loopback("localhost") && loopback("127.0.0.2") && loopback("[::1]"));
        assert!(!loopback("ghcr.io") && !loopback("10.0.0.1"));
    }
}
