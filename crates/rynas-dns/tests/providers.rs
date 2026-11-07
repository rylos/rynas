//! Provider clients against fake HTTP servers: the requests each one sends (method, path,
//! authentication, body) and how it reads the answers. No real provider is ever called.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use rynas_api_types::web_access::{DnsAction, DnsProvider};
use rynas_dns::plan::{apply, plan, wanted};
use rynas_dns::{Creds, DnsApi, DnsError, Endpoints, Rtype, client};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Req {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

type Handler = Box<dyn Fn(&Req, &mut State) -> (u16, String) + Send>;

/// Shared state of a fake provider: record sets by key.
#[derive(Default)]
struct State {
    sets: BTreeMap<String, Value>,
    next_id: u64,
}

struct Fake {
    base: String,
    reqs: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    fn start(handler: Handler) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let r2 = reqs.clone();
        std::thread::spawn(move || {
            let mut state = State::default();
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let mut rd = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if rd.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut headers = BTreeMap::new();
                loop {
                    let mut h = String::new();
                    rd.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                let len: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                rd.read_exact(&mut body).unwrap();
                let req = Req {
                    method,
                    path,
                    headers,
                    body: String::from_utf8_lossy(&body).into_owned(),
                };
                let (status, text) = handler(&req, &mut state);
                r2.lock().unwrap().push(req);
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Fake { base, reqs }
    }

    fn reqs(&self) -> Vec<Req> {
        self.reqs.lock().unwrap().clone()
    }

    fn api(&self, p: DnsProvider, creds: &[(&str, &str)]) -> Box<dyn DnsApi> {
        let c: Creds = creds
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut e = Endpoints::default();
        e.overrides.insert(p, self.base.clone());
        client(p, &c, &e).unwrap().unwrap()
    }
}

fn v4() -> Option<std::net::Ipv4Addr> {
    Some("192.0.2.10".parse().unwrap())
}

/// Runs plan + apply + plan again (all `keep`) for `domain`.
fn round_trip(api: &dyn DnsApi, domain: &str) {
    let w = wanted(domain, true, false, &[], v4(), None);
    let (zone, mut changes) = plan(domain, api, &w, &[], &[]).unwrap();
    assert!(
        changes.iter().all(|c| c.action == DnsAction::Create),
        "{changes:?}"
    );
    assert!(apply(api, &zone, &mut changes), "{changes:?}");
    let (_, again) = plan(domain, api, &w, &[], &[]).unwrap();
    assert!(
        again.iter().all(|c| c.action == DnsAction::Keep),
        "{again:?}"
    );
}

// ---------------------------------------------------------------------------------------------

fn cloudflare_handler() -> Handler {
    Box::new(|r: &Req, st: &mut State| {
        if r.headers.get("authorization").map(String::as_str) != Some("Bearer cf-token-123") {
            return (
                403,
                json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]}).to_string(),
            );
        }
        let ok = |result: Value| {
            (
                200,
                json!({"success": true, "errors": [], "result": result}).to_string(),
            )
        };
        let (path, query) = r.path.split_once('?').unwrap_or((&r.path, ""));
        match (r.method.as_str(), path) {
            ("GET", "/zones") => {
                if query.contains("name=example.com") {
                    ok(json!([{"id": "zone1", "name": "example.com"}]))
                } else {
                    ok(json!([]))
                }
            }
            ("GET", "/zones/zone1/dns_records") => {
                let name = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("name="))
                    .unwrap_or("")
                    .replace("%2A", "*");
                let ty = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("type="))
                    .unwrap_or("");
                let v: Vec<Value> = st
                    .sets
                    .values()
                    .filter(|x| x["name"] == name.as_str() && x["type"] == ty)
                    .cloned()
                    .collect();
                ok(json!(v))
            }
            ("POST", "/zones/zone1/dns_records") => {
                st.next_id += 1;
                let mut rec = r.json();
                assert_eq!(rec["proxied"], false);
                rec["id"] = json!(format!("r{}", st.next_id));
                st.sets.insert(format!("r{}", st.next_id), rec.clone());
                ok(rec)
            }
            (m, p) if p.starts_with("/zones/zone1/dns_records/") => {
                let id = p.rsplit('/').next().unwrap().to_string();
                if m == "DELETE" {
                    st.sets.remove(&id);
                } else {
                    let mut rec = r.json();
                    rec["id"] = json!(id.clone());
                    st.sets.insert(id, rec);
                }
                ok(json!({"id": "x"}))
            }
            _ => (
                404,
                json!({"success": false, "errors": [{"code": 7003, "message": "no route"}]})
                    .to_string(),
            ),
        }
    })
}

#[test]
fn cloudflare_records() {
    let fake = Fake::start(cloudflare_handler());
    let api = fake.api(DnsProvider::Cloudflare, &[("api_token", "cf-token-123")]);
    let z = api.zone("*.nas.example.com").unwrap();
    assert_eq!((z.id.as_str(), z.name.as_str()), ("zone1", "example.com"));
    round_trip(api.as_ref(), "nas.example.com");
    let posts: Vec<Req> = fake
        .reqs()
        .into_iter()
        .filter(|r| r.method == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].json()["name"], "*.nas.example.com");
    assert_eq!(posts[0].json()["content"], "192.0.2.10");
    assert_eq!(posts[0].json()["type"], "A");
    // update then delete
    api.set(&z, "nas.example.com", Rtype::A, &["192.0.2.20".into()], 300)
        .unwrap();
    assert!(fake.reqs().iter().any(|r| r.method == "PUT"));
    api.delete(&z, "nas.example.com", Rtype::A).unwrap();
    assert!(api.get(&z, "nas.example.com", Rtype::A).unwrap().is_none());

    // a wrong token: Auth, and the token never appears in the message
    let bad = fake.api(DnsProvider::Cloudflare, &[("api_token", "wrong-token-xyz")]);
    match bad.zone("nas.example.com") {
        Err(DnsError::Auth(m)) => {
            assert!(m.contains("Authentication error"), "{m}");
            assert!(!m.contains("wrong-token-xyz"));
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------

fn desec_handler() -> Handler {
    Box::new(|r: &Req, st: &mut State| {
        if r.headers.get("authorization").map(String::as_str) != Some("Token desec-tok") {
            return (401, json!({"detail": "Invalid token."}).to_string());
        }
        let p = r.path.as_str();
        if r.method == "GET" && p.starts_with("/domains/?owns_qname=") {
            return (
                200,
                json!([{"name": "example.com", "minimum_ttl": 3600}]).to_string(),
            );
        }
        if r.method == "PATCH" && p == "/domains/example.com/rrsets/" {
            for rr in r.json().as_array().unwrap() {
                let key = format!(
                    "{}/{}",
                    rr["subname"].as_str().unwrap(),
                    rr["type"].as_str().unwrap()
                );
                if rr["records"].as_array().unwrap().is_empty() {
                    st.sets.remove(&key);
                } else {
                    assert!(rr["ttl"].as_u64().unwrap() >= 3600);
                    st.sets.insert(key, rr.clone());
                }
            }
            return (200, "[]".into());
        }
        if r.method == "GET"
            && let Some(rest) = p.strip_prefix("/domains/example.com/rrsets/")
        {
            let mut it = rest.trim_end_matches('/').split('/');
            let sub = it.next().unwrap();
            let sub = if sub == "@" { "" } else { sub };
            let ty = it.next().unwrap();
            return match st.sets.get(&format!("{sub}/{ty}")) {
                Some(v) => (200, v.to_string()),
                None => (404, json!({"detail": "Not found."}).to_string()),
            };
        }
        (400, json!({"detail": "bad"}).to_string())
    })
}

#[test]
fn desec_records() {
    let fake = Fake::start(desec_handler());
    let api = fake.api(DnsProvider::Desec, &[("token", "desec-tok")]);
    let z = api.zone("*.nas.example.com").unwrap();
    assert_eq!(z.min_ttl, 3600);
    round_trip(api.as_ref(), "nas.example.com");
    let patches: Vec<Req> = fake
        .reqs()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(patches[0].json()[0]["subname"], "*.nas");
    assert_eq!(patches[0].json()[0]["ttl"], 3600);
    // the apex reads through "@"
    assert!(
        fake.reqs()
            .iter()
            .any(|r| r.path == "/domains/example.com/rrsets/nas/A/")
    );
    api.delete(&z, "*.nas.example.com", Rtype::A).unwrap();
    assert!(
        api.get(&z, "*.nas.example.com", Rtype::A)
            .unwrap()
            .is_none()
    );
    let bad = fake.api(DnsProvider::Desec, &[("token", "nope-nope")]);
    assert!(matches!(bad.zone("nas.example.com"), Err(DnsError::Auth(m)) if m == "Invalid token."));
}

// ---------------------------------------------------------------------------------------------

fn hetzner_handler() -> Handler {
    Box::new(|r: &Req, st: &mut State| {
        if r.headers.get("authorization").map(String::as_str) != Some("Bearer hz-token") {
            return (
                401,
                json!({"error": {"code": "unauthorized", "message": "unable to authenticate"}})
                    .to_string(),
            );
        }
        let p = r.path.as_str();
        match r.method.as_str() {
            "GET" if p.starts_with("/zones?name=") => {
                if p.ends_with("=example.com") {
                    (
                        200,
                        json!({"zones": [{"id": 42, "name": "example.com"}]}).to_string(),
                    )
                } else {
                    (200, json!({"zones": []}).to_string())
                }
            }
            "POST" if p == "/zones/42/rrsets" => {
                let v = r.json();
                let key = format!(
                    "{}/{}",
                    v["name"].as_str().unwrap(),
                    v["type"].as_str().unwrap()
                );
                st.sets.insert(key, v.clone());
                (201, json!({"rrset": v}).to_string())
            }
            "POST" if p.ends_with("/actions/set_records") => {
                let rest = p
                    .strip_prefix("/zones/42/rrsets/")
                    .unwrap()
                    .strip_suffix("/actions/set_records")
                    .unwrap();
                let key = rest.replace("%2A", "*");
                let e = st.sets.get_mut(&key).unwrap();
                e["records"] = r.json()["records"].clone();
                (201, json!({"action": {"id": 1}}).to_string())
            }
            "GET" | "DELETE" if p.starts_with("/zones/42/rrsets/") => {
                let key = p.strip_prefix("/zones/42/rrsets/").unwrap().to_string();
                if r.method == "DELETE" {
                    return match st.sets.remove(&key) {
                        Some(_) => (201, json!({"action": {"id": 2}}).to_string()),
                        None => (
                            404,
                            json!({"error": {"code": "not_found", "message": "not found"}})
                                .to_string(),
                        ),
                    };
                }
                match st.sets.get(&key) {
                    Some(v) => (200, json!({"rrset": v}).to_string()),
                    None => (
                        404,
                        json!({"error": {"code": "not_found", "message": "not found"}}).to_string(),
                    ),
                }
            }
            _ => (
                400,
                json!({"error": {"code": "invalid_input", "message": r.path}}).to_string(),
            ),
        }
    })
}

#[test]
fn hetzner_records() {
    let fake = Fake::start(hetzner_handler());
    let api = fake.api(DnsProvider::Hetzner, &[("api_token", "hz-token")]);
    let z = api.zone("*.nas.example.com").unwrap();
    assert_eq!(z.id, "42");
    round_trip(api.as_ref(), "nas.example.com");
    let created: Vec<Value> = fake
        .reqs()
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/zones/42/rrsets")
        .map(Req::json)
        .collect();
    assert_eq!(created[0]["name"], "*.nas");
    assert_eq!(created[0]["records"][0]["value"], "192.0.2.10");
    api.set(
        &z,
        "*.nas.example.com",
        Rtype::A,
        &["192.0.2.30".into()],
        300,
    )
    .unwrap();
    assert_eq!(
        api.get(&z, "*.nas.example.com", Rtype::A)
            .unwrap()
            .unwrap()
            .values,
        ["192.0.2.30"]
    );
    api.delete(&z, "*.nas.example.com", Rtype::A).unwrap();
    api.delete(&z, "*.nas.example.com", Rtype::A).unwrap();
}

// ---------------------------------------------------------------------------------------------

fn porkbun_handler() -> Handler {
    Box::new(|r: &Req, st: &mut State| {
        let b = r.json();
        if b["apikey"] != "pk1_key" || b["secretapikey"] != "sk1_secret" {
            return (
                400,
                json!({"status": "ERROR", "message": "Invalid API key. (002)"}).to_string(),
            );
        }
        let ok = |extra: Value| {
            let mut v = json!({"status": "SUCCESS"});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            (200, v.to_string())
        };
        let p = r.path.as_str();
        let key_of = |rest: &str| {
            let mut it = rest.splitn(3, '/');
            let _domain = it.next().unwrap();
            let ty = it.next().unwrap();
            let sub = it.next().unwrap_or("").replace("%2A", "*");
            format!("{sub}/{ty}")
        };
        if p == "/domain/listAll" {
            return ok(json!({"domains": [{"domain": "example.com"}]}));
        }
        if let Some(rest) = p.strip_prefix("/dns/retrieveByNameType/") {
            let recs: Vec<Value> = st.sets.get(&key_of(rest)).into_iter().cloned().collect();
            return ok(json!({"records": recs}));
        }
        if p == "/dns/create/example.com" {
            let key = format!(
                "{}/{}",
                b["name"].as_str().unwrap(),
                b["type"].as_str().unwrap()
            );
            assert!(b["ttl"].as_str().unwrap().parse::<u32>().unwrap() >= 600);
            st.sets.insert(
                key,
                json!({"id": "1", "content": b["content"], "ttl": b["ttl"]}),
            );
            return ok(json!({"id": 1}));
        }
        if let Some(rest) = p.strip_prefix("/dns/editByNameType/") {
            let k = key_of(rest);
            st.sets.insert(
                k,
                json!({"id": "1", "content": b["content"], "ttl": b["ttl"]}),
            );
            return ok(json!({}));
        }
        if let Some(rest) = p.strip_prefix("/dns/deleteByNameType/") {
            st.sets.remove(&key_of(rest));
            return ok(json!({}));
        }
        (
            400,
            json!({"status": "ERROR", "message": "unknown"}).to_string(),
        )
    })
}

#[test]
fn porkbun_records() {
    let fake = Fake::start(porkbun_handler());
    let api = fake.api(
        DnsProvider::Porkbun,
        &[("api_key", "pk1_key"), ("api_secret_key", "sk1_secret")],
    );
    let z = api.zone("*.nas.example.com").unwrap();
    assert_eq!(z.name, "example.com");
    round_trip(api.as_ref(), "nas.example.com");
    let create = fake
        .reqs()
        .into_iter()
        .find(|r| r.path == "/dns/create/example.com")
        .unwrap();
    assert_eq!(create.json()["name"], "*.nas");
    assert_eq!(create.json()["ttl"], "600");
    api.set(&z, "nas.example.com", Rtype::A, &["192.0.2.40".into()], 600)
        .unwrap();
    assert!(
        fake.reqs()
            .iter()
            .any(|r| r.path == "/dns/editByNameType/example.com/A/nas")
    );
    api.delete(&z, "nas.example.com", Rtype::A).unwrap();
    let bad = fake.api(
        DnsProvider::Porkbun,
        &[("api_key", "pk1_key"), ("api_secret_key", "sk1_wrong")],
    );
    match bad.zone("nas.example.com") {
        Err(DnsError::Api(m)) => {
            assert!(m.contains("Invalid API key"));
            assert!(!m.contains("sk1_wrong"));
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------

fn ovh_handler() -> Handler {
    Box::new(|r: &Req, st: &mut State| {
        if r.path == "/auth/time" {
            return (200, "1700000000".into());
        }
        // the signature must match what the client claims to have signed
        let ts = r
            .headers
            .get("x-ovh-timestamp")
            .cloned()
            .unwrap_or_default();
        assert_eq!(r.headers.get("x-ovh-application").unwrap(), "AK");
        assert_eq!(r.headers.get("x-ovh-consumer").unwrap(), "CK");
        let sig = r
            .headers
            .get("x-ovh-signature")
            .cloned()
            .unwrap_or_default();
        assert!(sig.starts_with("$1$") && sig.len() == 43, "{sig} at {ts}");
        let p = r.path.as_str();
        match r.method.as_str() {
            "GET" if p == "/domain/zone" => (200, json!(["example.com"]).to_string()),
            "GET" if p.starts_with("/domain/zone/example.com/record?") => {
                let q = p.split_once('?').unwrap().1;
                let ty = q
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("fieldType="))
                    .unwrap();
                let sub = q
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("subDomain="))
                    .unwrap()
                    .replace("%2A", "*");
                let ids: Vec<u64> = st
                    .sets
                    .iter()
                    .filter(|(_, v)| v["fieldType"] == ty && v["subDomain"] == sub.as_str())
                    .map(|(k, _)| k.parse().unwrap())
                    .collect();
                (200, json!(ids).to_string())
            }
            "GET" => {
                let id = p.rsplit('/').next().unwrap();
                (200, st.sets[id].to_string())
            }
            "POST" if p == "/domain/zone/example.com/record" => {
                st.next_id += 1;
                st.sets.insert(st.next_id.to_string(), r.json());
                (200, r.json().to_string())
            }
            "POST" if p == "/domain/zone/example.com/refresh" => (200, "null".into()),
            "PUT" => {
                let id = p.rsplit('/').next().unwrap().to_string();
                let e = st.sets.get_mut(&id).unwrap();
                e["target"] = r.json()["target"].clone();
                (200, "null".into())
            }
            "DELETE" => {
                let id = p.rsplit('/').next().unwrap().to_string();
                st.sets.remove(&id);
                (200, "null".into())
            }
            _ => (400, json!({"message": "bad"}).to_string()),
        }
    })
}

#[test]
fn ovh_records() {
    let fake = Fake::start(ovh_handler());
    let api = fake.api(
        DnsProvider::Ovh,
        &[
            ("application_key", "AK"),
            ("application_secret", "AS"),
            ("consumer_key", "CK"),
        ],
    );
    round_trip(api.as_ref(), "nas.example.com");
    let reqs = fake.reqs();
    assert_eq!(
        reqs.iter().filter(|r| r.path == "/auth/time").count(),
        1,
        "time asked once"
    );
    assert!(
        reqs.iter()
            .any(|r| r.path == "/domain/zone/example.com/refresh")
    );
    // the signature of one request, recomputed
    let one = reqs.iter().find(|r| r.path == "/domain/zone").unwrap();
    let ts: i64 = one.headers["x-ovh-timestamp"].parse().unwrap();
    assert_eq!(
        one.headers["x-ovh-signature"],
        rynas_dns::providers::ovh::signature(
            "AS",
            "CK",
            "GET",
            &format!("{}/domain/zone", fake.base),
            "",
            ts
        )
    );
    let z = api.zone("nas.example.com").unwrap();
    api.delete(&z, "*.nas.example.com", Rtype::A).unwrap();
    assert!(
        api.get(&z, "*.nas.example.com", Rtype::A)
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------------------------

#[test]
fn duckdns_records() {
    let fake = Fake::start(Box::new(|r: &Req, _st: &mut State| {
        if r.path.contains("token=duck-token") && r.path.contains("domains=mynas") {
            (200, "OK".into())
        } else {
            (200, "KO".into())
        }
    }));
    let api = fake.api(DnsProvider::Duckdns, &[("api_token", "duck-token")]);
    assert!(!api.readable());
    let w = wanted(
        "mynas.duckdns.org",
        false,
        true,
        &[],
        v4(),
        Some("fd00::10".parse().unwrap()),
    );
    let (zone, mut changes) = plan("mynas.duckdns.org", api.as_ref(), &w, &[], &[]).unwrap();
    assert!(changes.iter().all(|c| c.action == DnsAction::Update));
    assert!(apply(api.as_ref(), &zone, &mut changes));
    let paths: Vec<String> = fake.reqs().iter().map(|r| r.path.clone()).collect();
    assert_eq!(
        paths,
        [
            "/update?domains=mynas&token=duck-token&ip=192.0.2.10",
            "/update?domains=mynas&token=duck-token&ipv6=fd00%3A%3A10",
        ]
    );
    assert!(api.zone("nas.example.com").is_err());
    let bad = fake.api(DnsProvider::Duckdns, &[("api_token", "wrong-duck")]);
    let z = bad.zone("mynas.duckdns.org").unwrap();
    let e = bad
        .set(
            &z,
            "mynas.duckdns.org",
            Rtype::A,
            &["192.0.2.10".into()],
            60,
        )
        .unwrap_err();
    assert!(!e.to_string().contains("wrong-duck"), "{e}");
}

#[test]
fn unreachable_provider_is_a_network_error() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let mut e = Endpoints::default();
    e.overrides.insert(DnsProvider::Cloudflare, base);
    let c: Creds = [("api_token".to_string(), "tok-secret-1".to_string())].into();
    let api = client(DnsProvider::Cloudflare, &c, &e).unwrap().unwrap();
    match api.zone("nas.example.com") {
        Err(DnsError::Network(m)) => assert!(!m.contains("tok-secret-1")),
        other => panic!("{other:?}"),
    }
}
