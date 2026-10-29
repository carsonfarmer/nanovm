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

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail, ensure};
use http::HeaderMap;
use oci_client::manifest::{OciDescriptor, OciImageManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sigstore_trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, TrustedRoot};
use sigstore_verify::{VerificationPolicy, Verifier};

use crate::config::Limits;

pub const APP_TYPE: &str = "application/vnd.nanovm.app.v1+json";
const CONFIG_TYPE: &str = "application/vnd.nanovm.config.v1+json";
const WASM_TYPE: &str = "application/wasm";
const BUNDLE_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
const VERSION: &str = "org.opencontainers.image.version";

/// The headers the proxy names an app with. The node trusts them because
/// only the proxy reaches it; the proxy must drop any a client sends.
pub const APP_HEADER: &str = "nanovm-app";
pub const TRUST_HEADER: &str = "nanovm-trust";
pub const MIN_VERSION_HEADER: &str = "nanovm-min-version";

/// Which app version a request wants, and whose signature it accepts.
#[derive(Debug, Clone)]
pub struct AppRef {
    /// `registry/repository@sha256:…`
    pub reference: Reference,
    pub digest: String,
    /// `(issuer, subject)` pairs; a signature by any of them is accepted.
    pub trust: Vec<(String, String)>,
    pub min_version: Option<semver::Version>,
}

impl AppRef {
    /// `None` when the request names no registry app.
    pub fn from_headers(h: &HeaderMap) -> anyhow::Result<Option<AppRef>> {
        let get = |name| h.get(name).map(|v| v.to_str().with_context(|| format!("{name} is not text")));
        let Some(app) = get(APP_HEADER) else { return Ok(None) };
        let reference: Reference = app?.parse().context(APP_HEADER)?;
        let digest = reference.digest().context("nanovm-app must name a digest")?.to_string();
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
            reference,
            digest,
            trust,
            min_version,
        }))
    }
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
    client: Client,
    verifier: Verifier,
    /// Content-addressed: `<dir>/<digest>` holds a verified blob.
    dir: PathBuf,
}

impl Registry {
    pub fn new(dir: PathBuf) -> anyhow::Result<Registry> {
        let root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)?;
        std::fs::create_dir_all(&dir)?;
        Ok(Registry {
            client: Client::default(),
            verifier: Verifier::new(&root)?,
            dir,
        })
    }

    /// The app `r` names, if a signature by one of its trusted identities
    /// covers it and its version is at least the floor. Nothing but the
    /// manifest and the signatures is downloaded before that is settled.
    pub async fn load(&self, r: &AppRef) -> anyhow::Result<Pulled> {
        let manifest = self.manifest(r).await?;
        let version = manifest
            .annotations
            .as_ref()
            .and_then(|a| a.get(VERSION))
            .context("the manifest has no org.opencontainers.image.version")?;
        let version: semver::Version = version.parse().with_context(|| format!("version {version:?}"))?;
        self.verify(r).await?;
        if let Some(min) = &r.min_version {
            ensure!(&version >= min, "version {version} is below the minimum {min}");
        }
        ensure!(manifest.artifact_type.as_deref() == Some(APP_TYPE), "not a nanovm app ({APP_TYPE})");
        ensure!(manifest.config.media_type == CONFIG_TYPE, "config is not {CONFIG_TYPE}");
        let [layer] = &manifest.layers[..] else { bail!("an app has exactly one layer") };
        ensure!(layer.media_type == WASM_TYPE, "the layer is not {WASM_TYPE}");
        let spec = serde_json::from_slice(&self.blob(r, &manifest.config).await?).context("the app's config")?;
        Ok(Pulled {
            spec,
            wasm: self.blob(r, layer).await?,
            version,
        })
    }

    async fn manifest(&self, r: &AppRef) -> anyhow::Result<OciImageManifest> {
        let bytes = match self.cached(&r.digest) {
            Some(b) => b,
            None => {
                let accept = [oci_client::manifest::OCI_IMAGE_MEDIA_TYPE];
                let (bytes, _) = self.client.pull_manifest_raw(&r.reference, &RegistryAuth::Anonymous, &accept).await?;
                self.store(&r.digest, &bytes)?
            }
        };
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn blob(&self, r: &AppRef, d: &OciDescriptor) -> anyhow::Result<Vec<u8>> {
        if let Some(b) = self.cached(&d.digest) {
            return Ok(b);
        }
        let mut bytes = Vec::new();
        self.client.pull_blob(&r.reference, d, &mut bytes).await?;
        self.store(&d.digest, &bytes)
    }

    /// Checks the attached signatures against the trust policy. They are
    /// fetched every time: whether a signature exists is the registry's
    /// current answer, and they are small.
    async fn verify(&self, r: &AppRef) -> anyhow::Result<()> {
        let referrers = self.client.pull_referrers(&r.reference, Some(BUNDLE_TYPE)).await?;
        let hash = sigstore_types::Sha256Hash::from_hex(r.digest.trim_start_matches("sha256:"))?;
        let mut why = String::from("no signature");
        for entry in referrers.manifests {
            let reference = r.reference.clone_with_digest(entry.digest);
            let accept = [oci_client::manifest::OCI_IMAGE_MEDIA_TYPE];
            let (bytes, _) = self.client.pull_manifest_raw(&reference, &RegistryAuth::Anonymous, &accept).await?;
            let m: OciImageManifest = serde_json::from_slice(&bytes)?;
            for layer in m.layers.iter().filter(|l| l.media_type == BUNDLE_TYPE) {
                let mut json = Vec::new();
                self.client.pull_blob(&reference, layer, &mut json).await?;
                let bundle = sigstore_types::Bundle::from_json(std::str::from_utf8(&json)?)?;
                for (issuer, subject) in &r.trust {
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

    fn path(&self, digest: &str) -> PathBuf {
        self.dir.join(digest.replace(':', "-"))
    }

    fn cached(&self, digest: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(digest)).ok()
    }

    /// Keeps `bytes` if they hash to `digest`; a reader only ever sees a
    /// complete file.
    fn store(&self, digest: &str, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        let actual = format!("sha256:{:x}", Sha256::digest(bytes));
        ensure!(actual == digest, "the registry sent {actual} for {digest}");
        let path = self.path(digest);
        static WRITER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{n}"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(bytes.to_vec())
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

    /// A request without `nanovm-app` names no registry app, whatever else
    /// it carries.
    #[test]
    fn no_app_header_is_no_app() {
        assert!(AppRef::from_headers(&HeaderMap::new()).unwrap().is_none());
        let h = headers(&[(TRUST_HEADER, "garbage"), (MIN_VERSION_HEADER, "x")]);
        assert!(AppRef::from_headers(&h).unwrap().is_none());
    }

    /// The app is pinned by digest: a tag alone, an unparsable reference or
    /// a value that isn't text is refused.
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

    /// Trust is a comma-separated list of `<issuer> <subject>`, with any
    /// whitespace around and between them; every entry must be a pair.
    #[test]
    fn trust_is_a_list_of_issuer_subject_pairs() {
        let app = app();
        let h = headers(&[(APP_HEADER, &app), (TRUST_HEADER, " i1  s1 ,i2\ts2,  i3 s3  ")]);
        let r = AppRef::from_headers(&h).unwrap().unwrap();
        let pairs = [("i1", "s1"), ("i2", "s2"), ("i3", "s3")].map(|(i, s)| (i.to_string(), s.to_string()));
        assert_eq!(r.trust, pairs);
        assert_eq!(r.min_version, None);

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
        assert_eq!(r.min_version, Some(semver::Version::parse("1.2.3-rc.1").unwrap()));
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

    /// The cache keeps only bytes that hash to their digest, under a name
    /// without `:`, and leaves no temporary file behind.
    #[test]
    fn cache_keeps_only_matching_blobs() {
        let dir = std::env::temp_dir().join(format!("nanovm-registry-test-{}", std::process::id()));
        let reg = Registry::new(dir.clone()).unwrap();
        let digest = format!("sha256:{:x}", Sha256::digest(b"blob"));
        assert!(reg.store(&digest, b"other").is_err());
        assert_eq!(reg.cached(&digest), None);
        assert_eq!(reg.store(&digest, b"blob").unwrap(), b"blob");
        assert_eq!(reg.cached(&digest).as_deref(), Some(&b"blob"[..]));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [digest.replace(':', "-")]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
