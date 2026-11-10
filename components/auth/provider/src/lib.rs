//! Auth building blocks: JWT verification (local keys or a JWK Set fetched
//! through egress and cached in the volume), argon2id password hashing, and
//! server-side sessions in the volume.
//!
//! No internal write-ahead log: the host commits or rolls back each request's
//! changes as a whole. A session change touches one file: it is created whole
//! (removed again if the write is refused) or replaced by rename, so an error
//! return means nothing changed; if cleanup fails the component traps and the
//! request rolls back. The JWK Set cache is only an optimization: a refused
//! cache write is ignored.
use std::cell::RefCell;
use std::collections::HashMap;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha384, Sha512};
use util::wasip3::filesystem::types::{DescriptorFlags, DescriptorType, ErrorCode as FsError, OpenFlags};

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "provider" });
}
use bindings::exports::nanovm::auth::{jwt, passwords, sessions};
bindings::export!(Provider with_types_in bindings);
struct Provider;

const ROOT: &str = ".auth";
const SESSIONS: &str = ".auth/sessions";
const JWKS_DIR: &str = ".auth/jwks";
const MAX_JWKS: usize = 256 << 10;
const REFETCH_S: u64 = 60;

fn now_s() -> u64 {
    util::wasip3::clocks::system_clock::now().seconds.max(0) as u64
}

fn random(n: u64) -> Vec<u8> {
    util::wasip3::random::random::get_random_bytes(n)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn mkdirs(dirs: &[&str]) -> Result<(), FsError> {
    let data = util::mount("/data").ok_or(FsError::NoEntry)?;
    for dir in dirs {
        match data.create_directory_at(dir.to_string()).await {
            Ok(()) | Err(FsError::Exist) => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn read(path: &str) -> Result<Option<Vec<u8>>, FsError> {
    match util::read_file("/data", path).await {
        Ok(b) => Ok(Some(b)),
        Err(FsError::NoEntry) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Creates `path` with `bytes`, or leaves nothing.
async fn create(path: &str, bytes: Vec<u8>) -> Result<(), FsError> {
    let fd = util::open(
        "/data",
        path,
        OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
        DescriptorFlags::WRITE,
    )
    .await?;
    if let Err(e) = util::write_all(&fd, bytes, 0).await {
        drop(fd);
        unlink_or_trap(path).await;
        return Err(e);
    }
    Ok(())
}

/// Replaces `path` with `bytes` through a temporary sibling, or changes nothing.
async fn replace(path: &str, bytes: Vec<u8>) -> Result<(), FsError> {
    let tmp = format!("{path}.new");
    let result = async {
        let fd = util::open(
            "/data",
            &tmp,
            OpenFlags::CREATE | OpenFlags::TRUNCATE,
            DescriptorFlags::WRITE,
        )
        .await?;
        util::write_all(&fd, bytes, 0).await?;
        drop(fd);
        let data = util::mount("/data").unwrap();
        data.rename_at(tmp.clone(), data, path.to_string()).await
    }
    .await;
    if result.is_err() {
        unlink_or_trap(&tmp).await;
    }
    result
}

async fn unlink_or_trap(path: &str) {
    match util::mount("/data").unwrap().unlink_file_at(path.to_string()).await {
        Ok(()) | Err(FsError::NoEntry) => (),
        Err(e) => panic!("auth: cannot remove {path} after a failed change ({e:?}); the request must roll back"),
    }
}

/// The names of the regular files in `dir` (none if it doesn't exist).
async fn list(dir: &str) -> Result<Vec<String>, FsError> {
    let fd = match util::open("/data", dir, OpenFlags::DIRECTORY, DescriptorFlags::READ).await {
        Ok(fd) => fd,
        Err(FsError::NoEntry) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let (mut entries, done) = fd.read_directory();
    let mut names = Vec::new();
    while let Some(entry) = entries.next().await {
        if matches!(entry.type_, DescriptorType::RegularFile) {
            names.push(entry.name);
        }
    }
    drop(entries);
    done.await?;
    Ok(names)
}

// ---- JWT ------------------------------------------------------------------

use jwt::{Error as JwtError, Key, Options};

fn malformed(what: impl std::fmt::Display) -> JwtError {
    JwtError::Malformed(what.to_string())
}

fn decode_json(part: &str) -> Result<(Value, Vec<u8>), JwtError> {
    let bytes = B64.decode(part).map_err(|e| malformed(format!("base64url: {e}")))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| malformed(format!("JSON: {e}")))?;
    if !value.is_object() {
        return Err(malformed("not a JSON object"));
    }
    Ok((value, bytes))
}

/// The JWK `kty` (and `crv`) an algorithm needs.
fn family(alg: &str) -> Result<(&'static str, Option<&'static str>), JwtError> {
    Ok(match alg {
        "RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512" => ("RSA", None),
        "ES256" => ("EC", Some("P-256")),
        "ES384" => ("EC", Some("P-384")),
        "EdDSA" => ("OKP", Some("Ed25519")),
        // Never verify an HMAC token with a public key as its secret.
        "HS256" | "HS384" | "HS512" => {
            return Err(JwtError::Unsupported(format!("{alg} needs an hmac key, not a JWK Set")));
        }
        alg => return Err(JwtError::Unsupported(format!("algorithm {alg}"))),
    })
}

fn parse_jwks(bytes: &[u8]) -> Result<Vec<Value>, String> {
    let set: Value = serde_json::from_slice(bytes).map_err(|e| format!("JWK Set: {e}"))?;
    set["keys"]
        .as_array()
        .cloned()
        .ok_or_else(|| "JWK Set: no keys array".into())
}

/// The key in `keys` that may verify a token with this `kid` and `alg`.
fn select<'a>(keys: &'a [Value], kid: Option<&str>, alg: &str) -> Result<&'a Value, JwtError> {
    let (kty, crv) = family(alg)?;
    let key = match kid {
        Some(kid) => keys
            .iter()
            .find(|k| k["kid"].as_str() == Some(kid))
            .ok_or_else(|| JwtError::NoKey(format!("no key {kid}")))?,
        None => {
            let mut fit = keys
                .iter()
                .filter(|k| k["kty"].as_str() == Some(kty) && k["crv"].as_str() == crv);
            match (fit.next(), fit.next()) {
                (Some(k), None) => k,
                (None, _) => return Err(JwtError::NoKey(format!("no {kty} key"))),
                (Some(_), Some(_)) => return Err(JwtError::NoKey("several keys fit; the token names none".into())),
            }
        }
    };
    if key["kty"].as_str() != Some(kty) || key["crv"].as_str() != crv {
        return Err(JwtError::Unsupported(format!("{alg} with a {} key", key["kty"])));
    }
    if key.get("alg").is_some_and(|a| a.as_str() != Some(alg)) {
        return Err(JwtError::Unsupported(format!("{alg} with a key for {}", key["alg"])));
    }
    if key.get("use").is_some_and(|u| u.as_str() != Some("sig")) {
        return Err(JwtError::Unsupported("the key is not for signatures".into()));
    }
    Ok(key)
}

fn coordinate(key: &Value, name: &str) -> Result<Vec<u8>, JwtError> {
    let text = key[name]
        .as_str()
        .ok_or_else(|| malformed(format!("JWK without {name}")))?;
    B64.decode(text).map_err(|e| malformed(format!("JWK {name}: {e}")))
}

fn verify_jwk(key: &Value, alg: &str, signed: &[u8], sig: &[u8]) -> Result<(), JwtError> {
    use rsa::signature::Verifier;
    let bad = |_| JwtError::BadSignature;
    match alg {
        "ES256" => {
            let (x, y) = (coordinate(key, "x")?, coordinate(key, "y")?);
            if x.len() != 32 || y.len() != 32 {
                return Err(malformed("P-256 coordinates are 32 bytes"));
            }
            let point = p256::EncodedPoint::from_affine_coordinates(x[..].into(), y[..].into(), false);
            let vk = p256::ecdsa::VerifyingKey::from_encoded_point(&point).map_err(|_| malformed("bad P-256 key"))?;
            let sig = p256::ecdsa::Signature::from_slice(sig).map_err(bad)?;
            vk.verify(signed, &sig).map_err(bad)
        }
        "ES384" => {
            let (x, y) = (coordinate(key, "x")?, coordinate(key, "y")?);
            if x.len() != 48 || y.len() != 48 {
                return Err(malformed("P-384 coordinates are 48 bytes"));
            }
            let point = p384::EncodedPoint::from_affine_coordinates(x[..].into(), y[..].into(), false);
            let vk = p384::ecdsa::VerifyingKey::from_encoded_point(&point).map_err(|_| malformed("bad P-384 key"))?;
            let sig = p384::ecdsa::Signature::from_slice(sig).map_err(bad)?;
            vk.verify(signed, &sig).map_err(bad)
        }
        "EdDSA" => {
            let x: [u8; 32] = coordinate(key, "x")?
                .try_into()
                .map_err(|_| malformed("Ed25519 keys are 32 bytes"))?;
            let vk = ed25519_dalek::VerifyingKey::from_bytes(&x).map_err(|_| malformed("bad Ed25519 key"))?;
            let sig = ed25519_dalek::Signature::from_slice(sig).map_err(bad)?;
            vk.verify_strict(signed, &sig).map_err(bad)
        }
        _ => {
            let n = rsa::BigUint::from_bytes_be(&coordinate(key, "n")?);
            let e = rsa::BigUint::from_bytes_be(&coordinate(key, "e")?);
            if n.bits() < 2048 {
                return Err(JwtError::Unsupported("RSA keys under 2048 bits".into()));
            }
            let pk =
                rsa::RsaPublicKey::new_with_max_size(n, e, 8192).map_err(|e| malformed(format!("RSA key: {e}")))?;
            let pkcs1 = |sig: &[u8]| rsa::pkcs1v15::Signature::try_from(sig).map_err(bad);
            let pss = |sig: &[u8]| rsa::pss::Signature::try_from(sig).map_err(bad);
            match alg {
                "RS256" => rsa::pkcs1v15::VerifyingKey::<Sha256>::new(pk).verify(signed, &pkcs1(sig)?),
                "RS384" => rsa::pkcs1v15::VerifyingKey::<Sha384>::new(pk).verify(signed, &pkcs1(sig)?),
                "RS512" => rsa::pkcs1v15::VerifyingKey::<Sha512>::new(pk).verify(signed, &pkcs1(sig)?),
                "PS256" => rsa::pss::VerifyingKey::<Sha256>::new(pk).verify(signed, &pss(sig)?),
                "PS384" => rsa::pss::VerifyingKey::<Sha384>::new(pk).verify(signed, &pss(sig)?),
                _ => rsa::pss::VerifyingKey::<Sha512>::new(pk).verify(signed, &pss(sig)?),
            }
            .map_err(bad)
        }
    }
}

fn verify_hmac(alg: &str, secret: &[u8], signed: &[u8], sig: &[u8]) -> Result<(), JwtError> {
    use hmac::{Hmac, Mac};
    fn check<M: Mac + hmac::digest::KeyInit>(secret: &[u8], signed: &[u8], sig: &[u8]) -> Result<(), JwtError> {
        let mut mac = <M as Mac>::new_from_slice(secret).map_err(|_| malformed("bad secret"))?;
        mac.update(signed);
        mac.verify_slice(sig).map_err(|_| JwtError::BadSignature)
    }
    match alg {
        "HS256" => check::<Hmac<Sha256>>(secret, signed, sig),
        "HS384" => check::<Hmac<Sha384>>(secret, signed, sig),
        "HS512" => check::<Hmac<Sha512>>(secret, signed, sig),
        alg => Err(JwtError::Unsupported(format!(
            "{alg} needs a public key; an hmac secret verifies HS256/384/512"
        ))),
    }
}

fn check_claims(claims: &Value, o: &Options) -> Result<(), JwtError> {
    let now = now_s() as f64;
    let leeway = f64::from(o.leeway_s);
    let time = |name: &str| match claims.get(name) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| malformed(format!("{name} is not a number"))),
    };
    match time("exp")? {
        Some(exp) if now > exp + leeway => return Err(JwtError::Expired),
        None if o.require_exp => return Err(JwtError::Claim("exp is required".into())),
        _ => (),
    }
    if time("nbf")?.is_some_and(|nbf| now + leeway < nbf) {
        return Err(JwtError::NotYetValid);
    }
    if let Some(iss) = &o.issuer
        && claims["iss"].as_str() != Some(iss)
    {
        return Err(JwtError::Claim("iss".into()));
    }
    if let Some(aud) = &o.audience {
        let ok = match &claims["aud"] {
            Value::String(s) => s == aud,
            Value::Array(list) => list.iter().any(|a| a.as_str() == Some(aud)),
            _ => false,
        };
        if !ok {
            return Err(JwtError::Claim("aud".into()));
        }
    }
    Ok(())
}

struct Jwks {
    fetched: u64,
    keys: Vec<Value>,
}

thread_local! {
    static REMOTE: RefCell<HashMap<String, Jwks>> = RefCell::new(HashMap::new());
}

/// A GET through the app's egress allowlist; the body, up to `MAX_JWKS`.
async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    use util::wasip3::http::types::{Fields, Method, Request, Response, Scheme};
    use util::wasip3::wit_future;
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else {
        return Err("the URL must be http or https".into());
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (request, _sent) = Request::new(Fields::new(), None, trailers_rx, None);
    drop(trailers_tx);
    let _ = request.set_method(&Method::Get);
    let _ = request.set_scheme(Some(&scheme));
    let _ = request.set_authority(Some(authority));
    let _ = request.set_path_with_query(Some(path));
    let response = util::wasip3::http::client::send(request)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let status = response.get_status_code();
    let (result_tx, result_rx) = wit_future::new(|| Ok(()));
    let (mut body, _trailers) = Response::consume_body(response, result_rx);
    let mut bytes = Vec::with_capacity(16 << 10);
    loop {
        if bytes.len() == bytes.capacity() {
            bytes.reserve(16 << 10);
        }
        let before = bytes.len();
        let (_, b) = body.read(bytes).await;
        bytes = b;
        if bytes.len() > MAX_JWKS {
            return Err("JWK Set over 256 KiB".into());
        }
        if bytes.len() == before {
            break;
        }
    }
    drop(result_tx);
    if status != 200 {
        return Err(format!("status {status}"));
    }
    Ok(bytes)
}

fn cache_path(url: &str) -> String {
    format!("{JWKS_DIR}/{}", &hex(&Sha256::digest(url.as_bytes()))[..32])
}

/// Fetches the set, remembers it, and caches it in the volume (best effort).
async fn refresh_jwks(url: &str) -> Result<(), String> {
    let body = fetch(url).await?;
    let keys = parse_jwks(&body)?;
    let fetched = now_s();
    let file = json!({"url": url, "fetched": fetched, "jwks": String::from_utf8_lossy(&body)}).to_string();
    // Only an optimization: a refused write leaves the old cache, or none.
    wit_bindgen::block_on(async {
        if mkdirs(&[ROOT, JWKS_DIR]).await.is_ok() {
            let _ = replace(&cache_path(url), file.into_bytes()).await;
        }
    });
    REMOTE.with_borrow_mut(|r| r.insert(url.to_string(), Jwks { fetched, keys }));
    Ok(())
}

/// The cached set's age and keys, from memory or the volume.
async fn cached(url: &str) -> Option<(u64, Vec<Value>)> {
    if let Some(hit) = REMOTE.with_borrow(|r| r.get(url).map(|j| (j.fetched, j.keys.clone()))) {
        return Some(hit);
    }
    let file: Value = serde_json::from_slice(&read(&cache_path(url)).await.ok()??).ok()?;
    if file["url"].as_str() != Some(url) {
        return None;
    }
    let (fetched, keys) = (
        file["fetched"].as_u64()?,
        parse_jwks(file["jwks"].as_str()?.as_bytes()).ok()?,
    );
    REMOTE.with_borrow_mut(|r| {
        r.insert(
            url.to_string(),
            Jwks {
                fetched,
                keys: keys.clone(),
            },
        )
    });
    Some((fetched, keys))
}

async fn remote_key(src: &jwt::JwksSource, kid: Option<&str>, alg: &str) -> Result<Value, JwtError> {
    let now = now_s();
    let mut set = cached(&src.url).await;
    if set
        .as_ref()
        .is_none_or(|(fetched, _)| now >= fetched + u64::from(src.ttl_s))
    {
        match refresh_jwks(&src.url).await {
            Ok(()) => set = cached(&src.url).await,
            // A stale set beats none while the issuer is unreachable.
            Err(e) if set.is_none() => return Err(JwtError::Fetch(e)),
            Err(_) => (),
        }
    }
    let (fetched, keys) = set.ok_or_else(|| JwtError::Fetch("no JWK Set".into()))?;
    match select(&keys, kid, alg) {
        // A new kid may mean the issuer rotated keys: look again, once a minute.
        Err(JwtError::NoKey(_)) if now >= fetched + REFETCH_S && kid.is_some() => {
            refresh_jwks(&src.url).await.map_err(JwtError::Fetch)?;
            let (_, keys) = cached(&src.url)
                .await
                .ok_or_else(|| JwtError::Fetch("no JWK Set".into()))?;
            select(&keys, kid, alg).cloned()
        }
        other => other.cloned(),
    }
}

impl jwt::Guest for Provider {
    async fn verify(token: String, key: Key, options: Options) -> Result<String, JwtError> {
        let parts: Vec<&str> = token.split('.').collect();
        let [head, body, sig] = parts[..] else {
            return Err(malformed("a JWT has three parts"));
        };
        let (header, _) = decode_json(head)?;
        if header.get("crit").is_some() {
            return Err(malformed("crit headers are not supported"));
        }
        let alg = header["alg"].as_str().ok_or_else(|| malformed("no alg"))?;
        if alg == "none" || !(options.algorithms.is_empty() || options.algorithms.iter().any(|a| a == alg)) {
            return Err(JwtError::Unsupported(format!("algorithm {alg}")));
        }
        let kid = header["kid"].as_str();
        let signed = &token.as_bytes()[..head.len() + 1 + body.len()];
        let sig = B64.decode(sig).map_err(|e| malformed(format!("signature: {e}")))?;
        match &key {
            Key::Hmac(secret) => verify_hmac(alg, secret, signed, &sig)?,
            Key::Jwks(set) => {
                let keys = parse_jwks(set.as_bytes()).map_err(malformed)?;
                verify_jwk(select(&keys, kid, alg)?, alg, signed, &sig)?
            }
            Key::JwksUrl(src) => verify_jwk(&remote_key(src, kid, alg).await?, alg, signed, &sig)?,
        }
        let (claims, text) = decode_json(body)?;
        check_claims(&claims, &options)?;
        String::from_utf8(text).map_err(malformed)
    }

    fn sign_hs256(claims: String, secret: Vec<u8>) -> Result<String, JwtError> {
        use hmac::{Hmac, Mac};
        let value: Value = serde_json::from_str(&claims).map_err(malformed)?;
        if !value.is_object() {
            return Err(malformed("claims must be a JSON object"));
        }
        let signed = format!(
            "{}.{}",
            B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#),
            B64.encode(claims)
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).map_err(|_| malformed("bad secret"))?;
        mac.update(signed.as_bytes());
        Ok(format!("{signed}.{}", B64.encode(mac.finalize().into_bytes())))
    }
}

// ---- passwords ------------------------------------------------------------

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use passwords::{Error as PwError, Params as PwParams};

const DEFAULT_PARAMS: PwParams = PwParams {
    memory_kib: 19_456,
    iterations: 2,
};

fn argon_params(m: u32, t: u32, p: u32) -> Option<argon2::Params> {
    if !(8..=65_536).contains(&m) || !(1..=10).contains(&t) || !(1..=4).contains(&p) {
        return None;
    }
    argon2::Params::new(m, t, p, None).ok()
}

fn parse_hash(hash: &str) -> Result<(PasswordHash<'_>, argon2::Params), PwError> {
    let parsed = PasswordHash::new(hash).map_err(|e| PwError::InvalidHash(e.to_string()))?;
    if !matches!(parsed.algorithm.as_str(), "argon2id" | "argon2i" | "argon2d") {
        return Err(PwError::InvalidHash(format!("not argon2: {}", parsed.algorithm)));
    }
    // A stored hash must not make verification take unbounded memory or time.
    let params = argon2::Params::try_from(&parsed).map_err(|e| PwError::InvalidHash(e.to_string()))?;
    argon_params(params.m_cost(), params.t_cost(), params.p_cost())
        .ok_or_else(|| PwError::InvalidHash("params out of range".into()))?;
    Ok((parsed, params))
}

impl passwords::Guest for Provider {
    fn hash(password: String, params: Option<PwParams>) -> Result<String, PwError> {
        if password.len() > 4096 {
            return Err(PwError::InvalidArgument("passwords are at most 4 KiB".into()));
        }
        let p = params.unwrap_or(DEFAULT_PARAMS);
        let params = argon_params(p.memory_kib, p.iterations, 1)
            .ok_or_else(|| PwError::InvalidArgument("memory-kib is 8-65536, iterations 1-10".into()))?;
        let salt = SaltString::encode_b64(&random(16)).map_err(|e| PwError::InvalidArgument(e.to_string()))?;
        let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        argon
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| PwError::InvalidArgument(e.to_string()))
    }

    fn verify(password: String, hash: String) -> Result<bool, PwError> {
        let (parsed, _) = parse_hash(&hash)?;
        if password.len() > 4096 {
            return Ok(false);
        }
        match argon2::Argon2::default().verify_password(password.as_bytes(), &parsed) {
            Ok(()) => Ok(true),
            Err(argon2::password_hash::Error::Password) => Ok(false),
            Err(e) => Err(PwError::InvalidHash(e.to_string())),
        }
    }

    fn needs_rehash(hash: String, params: PwParams) -> Result<bool, PwError> {
        let (parsed, have) = parse_hash(&hash)?;
        Ok(parsed.algorithm.as_str() != "argon2id"
            || have.m_cost() < params.memory_kib
            || have.t_cost() < params.iterations)
    }
}

// ---- sessions -------------------------------------------------------------

use sessions::{Error as SessError, Session};

fn sess_err(e: FsError) -> SessError {
    match e {
        FsError::InsufficientSpace | FsError::Quota => SessError::Full,
        e => SessError::Other(format!("{e:?}")),
    }
}

/// Where a token's session lives; `None` for a string that can't be a token.
fn session_path(token: &str) -> Option<String> {
    (token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then(|| format!("{SESSIONS}/{}", &hex(&Sha256::digest(token.as_bytes()))[..32]))
}

fn decode_session(bytes: &[u8]) -> Result<Session, SessError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| SessError::Other(format!("corrupt session: {e}")))?;
    let field = |k: &str| v[k].clone();
    match (field("subject"), field("data"), field("created"), field("expires")) {
        (Value::String(subject), Value::String(data), Value::Number(c), Value::Number(e)) => Ok(Session {
            subject,
            data,
            created_s: c.as_u64().unwrap_or(0),
            expires_s: e.as_u64().unwrap_or(0),
        }),
        _ => Err(SessError::Other("corrupt session".into())),
    }
}

fn encode_session(s: &Session) -> Vec<u8> {
    json!({"subject": s.subject, "data": s.data, "created": s.created_s, "expires": s.expires_s})
        .to_string()
        .into_bytes()
}

fn check_ttl(ttl: u32) -> Result<(), SessError> {
    match (1..=315_360_000).contains(&ttl) {
        true => Ok(()),
        false => Err(SessError::InvalidArgument("ttl-s is 1 second to 10 years".into())),
    }
}

/// The live session at `path`, if any.
async fn load(path: &str) -> Result<Option<Session>, SessError> {
    let Some(bytes) = read(path).await.map_err(sess_err)? else {
        return Ok(None);
    };
    let s = decode_session(&bytes)?;
    Ok((s.expires_s > now_s()).then_some(s))
}

impl sessions::Guest for Provider {
    async fn create(subject: String, data: String, ttl_s: u32) -> Result<String, SessError> {
        if subject.is_empty() || subject.len() > 256 || data.len() > 64 << 10 {
            return Err(SessError::InvalidArgument(
                "subject is 1-256 bytes, data at most 64 KiB".into(),
            ));
        }
        check_ttl(ttl_s)?;
        let token = B64.encode(random(32));
        let now = now_s();
        let session = Session {
            subject,
            data,
            created_s: now,
            expires_s: now + u64::from(ttl_s),
        };
        let path = session_path(&token).unwrap();
        // Once started, a change runs to completion or cleanup even if the
        // caller stops waiting.
        wit_bindgen::block_on(async {
            mkdirs(&[ROOT, SESSIONS]).await.map_err(sess_err)?;
            create(&path, encode_session(&session)).await.map_err(sess_err)
        })?;
        Ok(token)
    }

    async fn get(token: String) -> Result<Option<Session>, SessError> {
        match session_path(&token) {
            Some(path) => load(&path).await,
            None => Ok(None),
        }
    }

    async fn refresh(token: String, ttl_s: u32) -> Result<bool, SessError> {
        check_ttl(ttl_s)?;
        let Some(path) = session_path(&token) else {
            return Ok(false);
        };
        let Some(mut s) = load(&path).await? else {
            return Ok(false);
        };
        s.expires_s = now_s() + u64::from(ttl_s);
        wit_bindgen::block_on(replace(&path, encode_session(&s))).map_err(sess_err)?;
        Ok(true)
    }

    async fn destroy(token: String) -> Result<bool, SessError> {
        let Some(path) = session_path(&token) else {
            return Ok(false);
        };
        wit_bindgen::block_on(async {
            match util::mount("/data").unwrap().unlink_file_at(path).await {
                Ok(()) => Ok(true),
                Err(FsError::NoEntry) => Ok(false),
                Err(e) => Err(sess_err(e)),
            }
        })
    }

    async fn destroy_subject(subject: String) -> Result<u32, SessError> {
        let mut doomed = Vec::new();
        for name in list(SESSIONS).await.map_err(sess_err)? {
            let path = format!("{SESSIONS}/{name}");
            if let Some(bytes) = read(&path).await.map_err(sess_err)?
                && decode_session(&bytes).is_ok_and(|s| s.subject == subject)
            {
                doomed.push(path);
            }
        }
        // All or none: once one is gone, a failure traps and rolls back.
        wit_bindgen::block_on(async {
            let data = util::mount("/data").unwrap();
            for (i, path) in doomed.iter().enumerate() {
                match data.unlink_file_at(path.clone()).await {
                    Ok(()) | Err(FsError::NoEntry) => (),
                    Err(e) if i == 0 => return Err(sess_err(e)),
                    Err(e) => panic!("auth: cannot end every session of a subject ({e:?}); the request must roll back"),
                }
            }
            Ok(doomed.len() as u32)
        })
    }

    async fn sweep(limit: u32) -> Result<u32, SessError> {
        let now = now_s();
        let mut removed = 0;
        let data = util::mount("/data").unwrap();
        for name in list(SESSIONS).await.map_err(sess_err)? {
            if removed == limit {
                break;
            }
            let path = format!("{SESSIONS}/{name}");
            let expired = match read(&path).await.map_err(sess_err)? {
                Some(bytes) => decode_session(&bytes).map_or(true, |s| s.expires_s <= now),
                None => false,
            };
            // Each removal stands alone; stop at the first refusal.
            if expired && wit_bindgen::block_on(data.unlink_file_at(path)).is_ok() {
                removed += 1;
            } else if expired {
                break;
            }
        }
        Ok(removed)
    }
}
