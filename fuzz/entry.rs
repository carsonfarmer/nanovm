//! Fuzz entry points, compiled into nanovm only with its `fuzzing` feature
//! (as `nanovm::fuzz`, from `lib.rs`), so they can reach the parsers without
//! making them public. Each takes the fuzzer's bytes, runs one parser on
//! input an attacker controls, and panics if a property the node relies on
//! fails. The targets in `fuzz_targets/` are one line each.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue};

/// WebSocket-over-HTTP events in an object's answer (`ws::parse`). Any bytes:
/// no panic, and what parses re-encodes, no longer, to the same events.
/// Events built from the bytes round-trip exactly.
pub fn ws_events(data: &[u8]) {
    use crate::ws::{Event, parse};
    let encode = |events: &[Event]| -> Vec<u8> { events.iter().flat_map(|e| e.encode()).collect() };
    if let Some(events) = parse(Bytes::copy_from_slice(data)) {
        let again = encode(&events);
        assert!(again.len() <= data.len(), "{events:?} re-encodes longer than {data:?}");
        assert_eq!(parse(Bytes::from(again)).as_ref(), Some(&events));
    }
    let mut events = vec![];
    let mut rest = data;
    while let Some((&kind, tail)) = rest.split_first() {
        let n = usize::from(tail.first().copied().unwrap_or(0)).min(tail.len().saturating_sub(1));
        let content = tail.get(1..1 + n).unwrap_or_default();
        rest = tail.get(1 + n..).unwrap_or_default();
        events.push(match kind % 6 {
            0 => Event::Open,
            1 => Event::Text(String::from_utf8_lossy(content).into_owned().into()),
            2 => Event::Binary(Bytes::copy_from_slice(content)),
            3 => Event::Close(None),
            4 => Event::Close(Some(u16::from_be_bytes([n as u8, kind]))),
            _ => Event::Disconnect,
        });
    }
    assert_eq!(parse(Bytes::from(encode(&events))), Some(events));
}

/// A guest's GRIP publish (`ws::messages`, parsed by its worker and again by
/// the shared proxy). No panic; at most 64 items; what it yields is no
/// bigger than the publish (N-72 bounds the parse's memory by that); and
/// where the pre-N-72 `serde_json::Value` parser also accepts the publish,
/// both yield the same messages.
pub fn grip_publish(data: &[u8]) {
    let typed = crate::ws::messages(data);
    assert_eq!(crate::ws::well_formed(data), typed.is_some());
    let Some(typed) = typed else { return };
    assert!(typed.len() <= 64);
    let size: usize = typed.iter().map(|(c, m)| c.len() + m.len()).sum();
    assert!(size <= data.len(), "{size} bytes of messages from a {}-byte publish", data.len());
    if let Some(generic) = value_messages(data) {
        assert_eq!(typed, generic, "the typed and generic parsers disagree on {:?}", String::from_utf8_lossy(data));
    }
}

/// `ws::messages` as it was before N-72 (573b7cc), as an oracle.
fn value_messages(json: &[u8]) -> Option<Vec<(String, tokio_tungstenite::tungstenite::Message)>> {
    use base64::Engine;
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::Message;
    let publish: Value = serde_json::from_slice(json).ok()?;
    let items = publish.get("items")?.as_array().filter(|i| i.len() <= 64)?;
    let message = |item: &Value| -> Option<Option<(String, Message)>> {
        let channel = item.get("channel")?.as_str()?.to_string();
        let Some(m) = item.get("formats")?.get("ws-message") else { return Some(None) };
        let m = m.as_object().filter(|m| m.len() == 1)?;
        let m = match (m.get("content"), m.get("content-bin")) {
            (Some(t), None) => Message::text(t.as_str()?),
            (None, Some(b)) => Message::binary(base64::engine::general_purpose::STANDARD.decode(b.as_str()?).ok()?),
            _ => return None,
        };
        Some(Some((channel, m)))
    };
    items.iter().map(message).collect::<Option<Vec<_>>>().map(|m| m.into_iter().flatten().collect())
}

/// A `_nanovm.<host>` TXT record's data, as the resolver returns it
/// (`proxy::unquote`, then `proxy::parse`). No panic; and a record the
/// proxy accepts is one the node accepts as the same app and policy: the
/// headers the proxy builds from it (`Proxy::resolve`) parse at the node
/// (`AppRef::from_headers`) to the app name the proxy keys sockets by,
/// the same trust pairs and the same version floor.
pub fn txt_record(data: &[u8]) {
    use crate::registry::{APP_HEADER, AppRef, HOST_HEADER, MIN_VERSION_HEADER, TRUST_HEADER, app_name};
    let Ok(text) = std::str::from_utf8(data) else { return };
    let txt = crate::proxy::unquote(text);
    let Ok(route) = crate::proxy::parse(&txt) else { return };
    assert!(route.app.tag().is_some() && route.app.digest().is_none(), "{txt:?}");
    let owner = "example.com";
    let app = format!("{}/{}@sha256:{}", route.app.registry(), route.app.repository(), "0".repeat(64));
    let mut headers = HeaderMap::new();
    for (k, v) in [(APP_HEADER, Some(app.as_str())), (HOST_HEADER, Some(owner)), (TRUST_HEADER, Some(route.trust.as_str())),
        (MIN_VERSION_HEADER, route.min.as_deref())] {
        if let Some(v) = v {
            // The proxy answers 502 for a value that isn't a header value.
            let Ok(v) = HeaderValue::from_str(v) else { return };
            headers.insert(k, v);
        }
    }
    let node = match AppRef::from_headers(&headers) {
        Ok(Some(node)) => node,
        Ok(None) => panic!("no app"),
        Err(e) => panic!("the node refuses what the proxy accepted from {txt:?}: {e:#}"),
    };
    assert_eq!(node.name(), app_name(owner, &route.app), "{txt:?}");
    let fields = txt.split(';').map(str::trim).filter_map(|f| f.strip_prefix("trust="));
    let pairs: Vec<(String, String)> = fields
        .map(|t| match t.split_whitespace().collect::<Vec<_>>()[..] {
            [issuer, subject] => (issuer.to_string(), subject.to_string()),
            _ => unreachable!("parse accepted {t:?}"),
        })
        .collect();
    assert_eq!(node.policy.trust, pairs, "{txt:?}");
    assert_eq!(node.policy.min_version.map(|v| v.to_string()), route.min, "{txt:?}");
}

/// The headers naming a request's app (`AppRef::from_headers`: OCI reference
/// parsing, digest, trust and version floor). The fuzzer's bytes are up to
/// four header values separated by 0xff: app, trust, min version, host.
/// No panic; an accepted reference names a well-formed sha256 digest, and
/// its app name is a valid name (one directory, cgroup and worker name):
/// at most 100 readable bytes, `-` and 16 hex digits.
pub fn app_ref(data: &[u8]) {
    use crate::registry::{APP_HEADER, AppRef, HOST_HEADER, MIN_VERSION_HEADER, TRUST_HEADER};
    let mut parts = data.split(|&b| b == 0xff);
    let mut headers = HeaderMap::new();
    for name in [APP_HEADER, TRUST_HEADER, MIN_VERSION_HEADER, HOST_HEADER] {
        if let Some(Ok(v)) = parts.next().map(HeaderValue::from_bytes) {
            headers.insert(name, v);
        }
    }
    let Ok(Some(r)) = AppRef::from_headers(&headers) else { return };
    let hex = r.digest.strip_prefix("sha256:").expect("a sha256 digest");
    assert!(hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "{}", r.digest);
    assert_eq!(r.reference.digest(), Some(r.digest.as_str()));
    // Parsing is cached per thread: the same text gives the same reference.
    assert_eq!(AppRef::from_headers(&headers).unwrap().unwrap().reference, r.reference);
    for (issuer, subject) in &r.policy.trust {
        assert!(!issuer.is_empty() && !subject.is_empty());
        assert!(!format!("{issuer}{subject}").contains([',', ' ', '\t']), "{issuer:?} {subject:?}");
    }
    // A host starting with `.` gives a name starting with `.` (N-506: only
    // the proxy sets the host, from a DNS owner name, which can't).
    let name = r.name();
    assert!(crate::table::valid_name(&name) || r.host.starts_with('.'), "app name {name:?} from {r:?}");
    assert!(!name.contains('/') && name.len() <= 117, "{name:?}");
}

/// An object's undo log (`journal::decode`, read back on recovery after a
/// crash). Any bytes: no panic, records start in order inside the log and
/// re-encode to exactly the bytes they came from. Records built from the
/// bytes round-trip, and every cut of their log (a crash mid-append)
/// decodes to exactly the records wholly before the cut; cancelling a
/// rename in place keeps the framing of the records after it.
pub fn journal_log(data: &[u8]) {
    use crate::journal::{Undo, decode, encode};
    use std::os::unix::ffi::OsStrExt;
    let mut next = 0;
    for (at, undo) in decode(data) {
        assert!(at as usize >= next && (at as usize) < data.len());
        if let Undo::Cancelled = undo {
            continue;
        }
        let bytes = encode(&undo);
        assert_eq!(data[at as usize..].get(..bytes.len()), Some(&bytes[..]));
        next = at as usize + bytes.len();
    }

    let path = |b: &[u8]| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b));
    let mut undos = vec![];
    let mut rest = data;
    while let Some((&kind, tail)) = rest.split_first() {
        let n = usize::from(tail.first().copied().unwrap_or(0)).min(tail.len().saturating_sub(1));
        let (a, b) = tail.get(1..1 + n).unwrap_or_default().split_at(n / 2);
        rest = tail.get(1 + n..).unwrap_or_default();
        let num = u64::from(kind).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        undos.push(match kind % 6 {
            0 => Undo::Size(path(a), num),
            1 => Undo::Bytes(path(a), num, b.to_vec()),
            2 => Undo::Created(path(a)),
            3 => Undo::RemovedDir(path(a)),
            _ => Undo::Renamed(path(a), path(b)),
        });
    }
    let mut log = vec![];
    let mut ends = vec![];
    let mut cancelled = vec![];
    for (i, undo) in undos.iter().enumerate() {
        let start = log.len();
        log.extend(encode(undo));
        ends.push(log.len());
        // `Journal::cancel_rename`: the tag is the 9th byte of a record.
        if matches!(undo, Undo::Renamed(..)) && data.get(i).is_some_and(|b| b & 1 == 1) {
            log[start + 8] = 5;
            cancelled.push(i);
        }
    }
    let decoded = decode(&log);
    assert_eq!(decoded.len(), undos.len());
    for (i, ((at, got), want)) in decoded.iter().zip(&undos).enumerate() {
        assert_eq!(*at as usize, if i == 0 { 0 } else { ends[i - 1] });
        if cancelled.contains(&i) {
            assert!(matches!(got, Undo::Cancelled));
        } else {
            assert_eq!(encode(got), encode(want));
        }
    }
    let step = (log.len() / 64).max(1);
    for cut in (0..=log.len()).step_by(step).chain([log.len().saturating_sub(1)]) {
        let whole = ends.iter().take_while(|&&e| e <= cut).count();
        assert_eq!(decode(&log[..cut]).len(), whole, "cut at {cut} of {}", log.len());
    }
}

/// A worker's OTLP batch, stamped by the supervisor (`telemetry::stamp`) at
/// the wire level. The first byte picks traces or logs. No panic; a stamped
/// batch that decodes carries only the stamped resource and no identity
/// attribute anywhere decoders look (whatever the input was); and when the
/// input decodes, the output equals decoding it, replacing every resource
/// and dropping identity attributes, as N-74's test checks for one batch.
pub fn telemetry_stamp(data: &[u8]) {
    use opentelemetry_proto::tonic::collector::{logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest};
    use opentelemetry_proto::tonic::common::v1::KeyValue;
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use prost::Message;
    let Some((&which, body)) = data.split_first() else { return };
    let traces = which & 1 == 0;
    let Ok(out) = crate::telemetry::stamp(if traces { "traces" } else { "logs" }, body, "app", "digest") else { return };
    let trusted = Resource {
        attributes: opentelemetry_proto::transform::common::tonic::Attributes::from(crate::telemetry::identity("app", "digest")).0,
        ..Default::default()
    };
    let forged = |kv: &KeyValue| kv.key_strindex != 0 || matches!(kv.key.as_str(), "service.name" | "service.version" | "nanovm.app");
    let clean = |a: &[KeyValue]| assert!(!a.iter().any(forged), "identity attribute survived: {a:?}");
    let scrub = |a: &mut Vec<KeyValue>| a.retain(|kv| !forged(kv));
    if traces {
        let Ok(got) = ExportTraceServiceRequest::decode(&*out) else {
            assert!(ExportTraceServiceRequest::decode(body).is_err(), "a decodable batch was stamped undecodable");
            return;
        };
        for r in &got.resource_spans {
            assert_eq!(r.resource.as_ref(), Some(&trusted));
            for s in &r.scope_spans {
                s.scope.iter().for_each(|s| clean(&s.attributes));
                for span in &s.spans {
                    clean(&span.attributes);
                    span.events.iter().for_each(|e| clean(&e.attributes));
                    span.links.iter().for_each(|l| clean(&l.attributes));
                }
            }
        }
        let Ok(mut expected) = ExportTraceServiceRequest::decode(body) else { return };
        for r in &mut expected.resource_spans {
            r.resource = Some(trusted.clone());
            for s in &mut r.scope_spans {
                s.scope.iter_mut().for_each(|s| scrub(&mut s.attributes));
                for span in &mut s.spans {
                    scrub(&mut span.attributes);
                    span.events.iter_mut().for_each(|e| scrub(&mut e.attributes));
                    span.links.iter_mut().for_each(|l| scrub(&mut l.attributes));
                }
            }
        }
        assert_eq!(got, expected);
    } else {
        let Ok(got) = ExportLogsServiceRequest::decode(&*out) else {
            assert!(ExportLogsServiceRequest::decode(body).is_err(), "a decodable batch was stamped undecodable");
            return;
        };
        for r in &got.resource_logs {
            assert_eq!(r.resource.as_ref(), Some(&trusted));
            for s in &r.scope_logs {
                s.scope.iter().for_each(|s| clean(&s.attributes));
                s.log_records.iter().for_each(|l| clean(&l.attributes));
            }
        }
        let Ok(mut expected) = ExportLogsServiceRequest::decode(body) else { return };
        for r in &mut expected.resource_logs {
            r.resource = Some(trusted.clone());
            for s in &mut r.scope_logs {
                s.scope.iter_mut().for_each(|s| scrub(&mut s.attributes));
                s.log_records.iter_mut().for_each(|l| scrub(&mut l.attributes));
            }
        }
        assert_eq!(got, expected);
    }
}
