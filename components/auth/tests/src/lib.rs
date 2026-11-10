//! A minimal app over the auth component, driven by
//! crates/nanovm/tests/components.rs. Probes panic on violated expectations.
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::nanovm::auth::jwt::{self, JwksSource, Key, Options};
use bindings::nanovm::auth::{passwords, sessions};
use serde_json::{Value, json};
use util::wasip3::http::types::{ErrorCode, Request, Response};
util::wasip3::http::service::export!(App);
struct App;

/// Generated once by a script (RSA-2048, P-256 and Ed25519 keys; far-future expiry).
const VECTORS: &str = include_str!("vectors.json");

fn vector(name: &str) -> String {
    let v: Value = serde_json::from_str(VECTORS).unwrap();
    v[name].as_str().unwrap().to_string()
}

fn opts() -> Options {
    Options {
        algorithms: vec![],
        issuer: None,
        audience: None,
        leeway_s: 0,
        require_exp: false,
    }
}

fn err<E: std::fmt::Debug>(e: E) -> Result<Response, ErrorCode> {
    util::json(409, json!({"error": format!("{e:?}")}).to_string())
}

impl util::wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let token = || req.q("token").unwrap_or("").to_string();
        let out = match req.path.as_str() {
            "/jwt-contracts" => {
                let jwks = || Key::Jwks(vector("jwks"));
                let verify = |name: &str, key: Key, o: Options| jwt::verify(vector(name), key, o);
                for name in ["rs256", "es256", "eddsa"] {
                    let claims = verify(name, jwks(), opts())
                        .await
                        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
                    assert_eq!(serde_json::from_str::<Value>(&claims).unwrap()["sub"], "alice");
                }
                let strict = Options {
                    algorithms: vec!["RS256".into(), "EdDSA".into()],
                    issuer: Some("https://issuer.test".into()),
                    audience: Some("app".into()),
                    leeway_s: 5,
                    require_exp: true,
                };
                verify("eddsa", jwks(), strict.clone()).await.expect("strict");
                let fails = [
                    ("es256", jwks(), strict.clone(), "Unsupported"),
                    (
                        "rs256",
                        jwks(),
                        Options {
                            issuer: Some("other".into()),
                            ..opts()
                        },
                        "Claim(\"iss\")",
                    ),
                    (
                        "rs256",
                        jwks(),
                        Options {
                            audience: Some("nope".into()),
                            ..opts()
                        },
                        "Claim(\"aud\")",
                    ),
                    // The set says rsa1 is for RS256.
                    ("ps256", jwks(), opts(), "Unsupported"),
                    ("expired", jwks(), opts(), "Expired"),
                    ("future", jwks(), opts(), "NotYetValid"),
                    ("unknown_kid", jwks(), opts(), "NoKey"),
                    ("confused", jwks(), opts(), "Unsupported"),
                    ("none", jwks(), opts(), "Unsupported"),
                    ("tampered", jwks(), opts(), "BadSignature"),
                    ("rs256", Key::Hmac(b"secret".to_vec()), opts(), "Unsupported"),
                ];
                for (name, key, o, want) in fails {
                    let got = format!("{:?}", verify(name, key, o).await.expect_err(name));
                    assert!(got.contains(want), "{name}: {got}, wanted {want}");
                }
                // Without its alg pin, the RSA key verifies PS256 too.
                let open = vector("jwks").replace(r#""alg":"RS256","#, "");
                verify("ps256", Key::Jwks(open), opts()).await.expect("ps256");
                // HS256 round trip; a wrong secret or a changed byte fails.
                let claims = r#"{"sub":"bob","exp":4102444800}"#;
                let t = jwt::sign_hs256(claims, b"s3cret").expect("sign");
                assert_eq!(
                    jwt::verify(t.clone(), Key::Hmac(b"s3cret".to_vec()), opts())
                        .await
                        .expect("hs256"),
                    claims
                );
                let wrong = jwt::verify(t.clone(), Key::Hmac(b"other".to_vec()), opts()).await;
                assert!(matches!(wrong, Err(jwt::Error::BadSignature)), "{wrong:?}");
                assert!(matches!(
                    jwt::verify(t, jwks(), opts()).await,
                    Err(jwt::Error::Unsupported(_))
                ));
                for bad in ["", "a.b", "a.b.c.d", "!!.e30.x", "e30.e30.@@"] {
                    let got = jwt::verify(bad.into(), jwks(), opts()).await;
                    assert!(matches!(got, Err(jwt::Error::Malformed(_))), "{bad}: {got:?}");
                }
                json!("ok")
            }
            // Verifies vector `case` against a JWK Set at `url`.
            "/jwks-url" => {
                let key = Key::JwksUrl(JwksSource {
                    url: req.q("url").unwrap().into(),
                    ttl_s: req.q_u64("ttl", 3600) as u32,
                });
                match jwt::verify(vector(req.q("case").unwrap()), key, opts()).await {
                    Ok(claims) => serde_json::from_str(&claims).unwrap(),
                    Err(e) => return err(e),
                }
            }
            "/password" => {
                let pw = String::from_utf8(req.body.clone()).unwrap();
                let params = req.q("m").map(|m| passwords::Params {
                    memory_kib: m.parse().unwrap(),
                    iterations: req.q_u64("t", 2) as u32,
                });
                match req.q("hash") {
                    Some(hash) => json!({"ok": passwords::verify(&pw, hash).expect("verify")}),
                    None => json!({"hash": passwords::hash(&pw, params).expect("hash")}),
                }
            }
            "/password-contracts" => {
                let p = |m, t| passwords::Params {
                    memory_kib: m,
                    iterations: t,
                };
                let h = passwords::hash("correct horse", Some(p(64, 1))).expect("hash");
                assert!(h.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{h}");
                assert!(passwords::verify("correct horse", &h).expect("verify"));
                assert!(!passwords::verify("wrong horse", &h).expect("verify"));
                assert_ne!(
                    passwords::hash("correct horse", Some(p(64, 1))).unwrap(),
                    h,
                    "salts differ"
                );
                assert!(passwords::needs_rehash(&h, p(19_456, 2)).unwrap());
                assert!(!passwords::needs_rehash(&h, p(64, 1)).unwrap());
                for (m, t) in [(4, 1), (65_537, 1), (64, 0), (64, 11)] {
                    assert!(matches!(
                        passwords::hash("x", Some(p(m, t))),
                        Err(passwords::Error::InvalidArgument(_))
                    ));
                }
                assert!(passwords::hash(&"x".repeat(4097), None).is_err());
                // A stored hash can't demand 4 GiB.
                let huge = "$argon2id$v=19$m=4194304,t=1,p=1$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaA";
                assert!(matches!(
                    passwords::verify("x", huge),
                    Err(passwords::Error::InvalidHash(_))
                ));
                assert!(matches!(
                    passwords::verify("x", "$2b$10$abc"),
                    Err(passwords::Error::InvalidHash(_))
                ));
                json!("ok")
            }
            "/session/create" => {
                let data = String::from_utf8(req.body.clone()).unwrap();
                match sessions::create(
                    req.q("subject").unwrap_or("alice").into(),
                    data,
                    req.q_u64("ttl", 3600) as u32,
                )
                .await
                {
                    Ok(t) => json!({"token": t}),
                    Err(e) => return err(e),
                }
            }
            "/session/get" => match sessions::get(token()).await.expect("get") {
                Some(s) => json!({"subject": s.subject, "data": s.data, "ttl": s.expires_s - s.created_s}),
                None => Value::Null,
            },
            "/session/refresh" => match sessions::refresh(token(), req.q_u64("ttl", 3600) as u32).await {
                Ok(live) => json!(live),
                Err(e) => return err(e),
            },
            "/session/destroy" => json!(sessions::destroy(token()).await.expect("destroy")),
            "/session/destroy-subject" => json!(
                sessions::destroy_subject(req.q("subject").unwrap().into())
                    .await
                    .expect("destroy")
            ),
            "/session/sweep" => json!(sessions::sweep(req.q_u64("limit", 100) as u32).await.expect("sweep")),
            // A failed request: its session never existed.
            "/session/fail" => {
                let t = sessions::create("ghost".into(), String::new(), 3600)
                    .await
                    .expect("create");
                util::write_file("/data", "ghost-token", t.into_bytes())
                    .await
                    .expect("note");
                return Err(ErrorCode::InternalError(Some("rollback probe".into())));
            }
            // For the bench: verify one vector against the inline set.
            "/verify" => {
                let claims = jwt::verify(vector(req.q("case").unwrap()), Key::Jwks(vector("jwks")), opts()).await;
                serde_json::from_str(&claims.expect("verify")).unwrap()
            }
            // For the bench: one session whose token is noted in the volume.
            "/session/fixed" => {
                let t = sessions::create("bench".into(), String::new(), 3600)
                    .await
                    .expect("create");
                util::write_file("/data", "bench-token", t.into_bytes())
                    .await
                    .expect("note");
                json!({})
            }
            "/session/get-fixed" => {
                let t = String::from_utf8(util::read_file("/data", "bench-token").await.expect("note")).unwrap();
                json!(sessions::get(t).await.expect("get").is_some())
            }
            "/session/contracts" => {
                assert!(sessions::create(String::new(), String::new(), 60).await.is_err());
                assert!(sessions::create("a".into(), String::new(), 0).await.is_err());
                assert!(sessions::create("a".into(), "d".repeat(65 << 10), 60).await.is_err());
                for bad in ["", "short", &"!".repeat(43), &"a".repeat(44)] {
                    assert!(sessions::get(bad.into()).await.expect("get").is_none());
                    assert!(!sessions::destroy(bad.into()).await.expect("destroy"));
                }
                let t = sessions::create("carol".into(), "{\"role\":\"admin\"}".into(), 60)
                    .await
                    .expect("create");
                assert_eq!(t.len(), 43);
                assert!(sessions::get(t.clone()).await.expect("get").is_some());
                json!("ok")
            }
            _ => return util::text(404, "no such route"),
        };
        util::json(200, out.to_string())
    }
}
