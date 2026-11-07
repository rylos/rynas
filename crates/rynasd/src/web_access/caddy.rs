//! The Caddy renderer: the whole engine configuration as one JSON document, loaded with
//! `POST /load` on the admin socket (the engine validates it and keeps the running one on
//! error). Field names follow Caddy 2.11's JSON structure:
//!
//! - admin on a Unix socket with a permission mode: `unix//path|0600`
//!   (https://caddyserver.com/docs/conventions#network-addresses), no autosave
//!   (`admin.config.persist: false`, https://caddyserver.com/docs/json/admin/config/);
//! - access rules with the `remote_ip` matcher (the peer address: Caddy is the edge, so no
//!   trusted proxies; https://caddyserver.com/docs/json/apps/http/servers/routes/match/remote_ip/)
//!   inside a `not` matcher, and `static_response` with `abort` (the connection is closed,
//!   no 403 page; https://caddyserver.com/docs/json/apps/http/servers/routes/handle/static_response/);
//! - `request_body.max_size` in bytes
//!   (https://caddyserver.com/docs/json/apps/http/servers/routes/handle/request_body/);
//! - `reverse_proxy` with `flush_interval: -1` for unbuffered routes, `stream_close_delay` so
//!   WebSockets survive a reload for a while, and the HTTP transport's `dial_timeout`,
//!   `read_timeout` and `tls.insecure_skip_verify`
//!   (https://caddyserver.com/docs/json/apps/http/servers/routes/handle/reverse_proxy/);
//! - certificates from files (`tls.certificates.load_files`,
//!   https://caddyserver.com/docs/json/apps/tls/certificates/load_files/) with automatic HTTPS
//!   off on both servers (`automatic_https.disable`), so the engine never orders a
//!   certificate in W1, and TLS on the HTTPS server through `tls_connection_policies`.
//!
//! - own domains (wave W2): `tls.certificates.automate` lists the ACME subjects and one
//!   `tls.automation.policies` entry per domain carries its ACME issuer
//!   (https://caddyserver.com/docs/json/apps/tls/automation/policies/issuers/acme/): the CA
//!   (Let's Encrypt or its staging), the optional `email` and `profile`, the DNS challenge
//!   with the caddy-dns provider module (`challenges.dns.provider`, secrets as `{file./path}`
//!   placeholders, Caddy ≥ 2.8), public `resolvers` for the propagation check and
//!   `override_domain` for a delegated `_acme-challenge`; HTTP-01 and TLS-ALPN off unless the
//!   domain asks for HTTP-01. ARI is on by default since Caddy 2.8 (certmagic 0.21).
//!
//! - imported certificates (§5.5): each valid pair that serves a name joins `load_files`
//!   with the tag `rynas-import-<id>`, and the HTTPS server gets one TLS connection policy per
//!   such pair before the catch-all `{}`: `match.sni` (the names it serves) and
//!   `certificate_selection.any_tag` (`modules/caddytls/connpolicy.go`,
//!   `certselection.go`), so certmagic picks it whatever else its cache holds for the name
//!   (its default selector takes the first valid match, in cache order). ACME subjects an
//!   import replaces are left out of `automate` and of the domain's policy until the import
//!   expires soon (< 14 days): then they are ordered while the policy keeps serving the
//!   import, so its expiry finds the ACME certificate in the cache. A hand-made config of
//!   this shape (tagged `load_files`, `match.sni` + `any_tag`) was accepted by rynas-caddy
//!   2.11.9's `caddy validate` (2026-11-07).
//!
//! - extra HTTPS ports (§5.6): one more server `rynas_port_<port>` per port, `listen
//!   [":<port>"]` (listener addresses are unique across servers, `modules/caddyhttp/server.go`
//!   `Listen`), with the same protocols, `strict_sni_host` (it compares the host without the
//!   port), TLS connection policies and abort catch-all as `rynas_https`: a certificate
//!   covers a name on any port. Each route's site carries the same `remote_ip` rule as on the
//!   main port; routes sharing the NAS's names on a port are ordered by path prefix, longest
//!   first (`path` matcher `[<p>, <p>/*]`). `@id` `rynas-route-<id>:<port>`.
//! - custom upstreams (§5.6): any `host:port`; an `https` upstream elsewhere than loopback is
//!   verified (`transport.tls: {}`) unless `skip_verify`; `host_header: upstream` sets
//!   `Host: {http.reverse_proxy.upstream.hostport}`
//!   (https://caddyserver.com/docs/caddyfile/directives/reverse_proxy#https). WebSockets
//!   need nothing: `reverse_proxy` passes upgrades.
//!
//! The source of the field names is Caddy v2.11.9's code (`modules/caddyhttp/...`,
//! `admin.go`, `modules/caddytls/acmeissuer.go`); the golden tests pin the output.

use serde_json::{Value, json};

use std::collections::BTreeSet;
use std::net::IpAddr;

use rynas_api_types::web_access::{
    ChallengeKind, HeaderPreset, HostHeader, ProxyMode, UpstreamScheme, WebDomain, WebRoute,
};

use super::domains::{self, PROPAGATION_RESOLVERS};
use super::imported::{self, Active};
use super::model::{access_ranges, parse_size};
use super::{RenderInput, Resolved};

/// Tag of the certificate rynasd issues for the local names.
pub const LOCAL_CERT_TAG: &str = "rynas-local";
/// How long upgraded connections (WebSockets) stay open after a reload.
pub const STREAM_CLOSE_DELAY: &str = "5m";

/// The engine-neutral render step: the same route model could feed another engine (an
/// Angie/nginx renderer is the documented contingency, D-PROXY).
pub trait Renderer {
    /// The engine configuration; `Value::Null` when the engine must not run (external mode).
    fn render(&self, input: &RenderInput, resolved: &Resolved) -> Value;
}

pub struct Caddy;

/// Identity headers no client may send to an upstream (rynas SSO sets them in W3).
const IDENTITY_HEADERS: &[&str] = &[
    "Remote-User",
    "Remote-Groups",
    "Remote-Email",
    "Remote-Name",
];

fn baseline_headers(preset: HeaderPreset) -> Vec<Value> {
    let mut v = vec![json!({
        "handler": "headers",
        "response": {"delete": ["Server", "X-Powered-By"], "deferred": true}
    })];
    let mut set = vec![
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "strict-origin-when-cross-origin"),
    ];
    if preset == HeaderPreset::Default {
        set.push(("X-Frame-Options", "SAMEORIGIN"));
    }
    // each one only when the upstream did not send its own (a `null` list matches an
    // absent header)
    for (name, value) in set {
        v.push(json!({
            "handler": "headers",
            "response": {
                "set": {name: [value]},
                "require": {"headers": {name: null}}
            }
        }));
    }
    v
}

fn dial(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// TLS towards the upstream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UpTls {
    Off,
    /// Any certificate (loopback containers, `skip_verify`).
    Insecure,
    /// Verified against the system roots.
    Verify,
}

fn proxy(
    dial_to: String,
    tls: UpTls,
    timeout: Option<&str>,
    buffering: bool,
    upstream_host: bool,
) -> Value {
    let mut transport = json!({"protocol": "http", "dial_timeout": "5s"});
    if let Some(t) = timeout {
        transport["read_timeout"] = json!(t);
    }
    match tls {
        UpTls::Off => {}
        // the container's own certificate, on loopback
        UpTls::Insecure => transport["tls"] = json!({"insecure_skip_verify": true}),
        UpTls::Verify => transport["tls"] = json!({}),
    }
    let mut request = json!({"delete": IDENTITY_HEADERS});
    if upstream_host {
        request["set"] = json!({"Host": ["{http.reverse_proxy.upstream.hostport}"]});
    }
    let mut p = json!({
        "handler": "reverse_proxy",
        "upstreams": [{"dial": dial_to}],
        "transport": transport,
        "stream_close_delay": STREAM_CLOSE_DELAY,
        "headers": {"request": request}
    });
    if !buffering {
        p["flush_interval"] = json!(-1);
    }
    p
}

/// One host-matched route: sources outside `ranges` are aborted, the rest handled.
fn site(id: &str, names: &[String], ranges: &[String], handle: Vec<Value>) -> Value {
    site_at(id, names, None, ranges, handle)
}

/// [`site`] limited to a path prefix (`/p` and `/p/...`).
fn site_at(
    id: &str,
    names: &[String],
    prefix: Option<&str>,
    ranges: &[String],
    handle: Vec<Value>,
) -> Value {
    let mut inner = Vec::new();
    if !ranges.is_empty() {
        inner.push(json!({
            "match": [{"not": [{"remote_ip": {"ranges": ranges}}]}],
            "handle": [{"handler": "static_response", "abort": true}]
        }));
    }
    inner.push(json!({"handle": handle}));
    let mut m = json!({"host": names});
    if let Some(p) = prefix {
        m["path"] = json!([p, format!("{p}/*")]);
    }
    json!({
        "@id": format!("rynas-{id}"),
        "match": [m],
        "handle": [{"handler": "subroute", "routes": inner}],
        "terminal": true
    })
}

fn route_handlers(r: &WebRoute) -> Vec<Value> {
    let mut h = baseline_headers(r.headers);
    if let Some(max) = r.limits.body.as_deref().and_then(parse_size) {
        h.push(json!({"handler": "request_body", "max_size": max}));
    }
    let loopback = r
        .upstream
        .host
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    let tls = match r.upstream.scheme {
        UpstreamScheme::Http => UpTls::Off,
        UpstreamScheme::Https if loopback || r.upstream.skip_verify => UpTls::Insecure,
        UpstreamScheme::Https => UpTls::Verify,
    };
    h.push(proxy(
        dial(&r.upstream.host, r.upstream.port),
        tls,
        r.limits.timeout.as_deref(),
        r.limits.buffering,
        r.upstream.host_header == HostHeader::Upstream,
    ));
    h
}

/// The route's site on its extra `port`: its names and, with `names.nas`, the NAS's.
fn port_site(r: &super::ResolvedRoute, port: u16, input: &RenderInput) -> Value {
    site_at(
        &format!("route-{}:{port}", r.route.id),
        &r.port_names(),
        r.route.prefix.as_deref(),
        &access_ranges(r.route.access, input.facts, &r.route.allow),
        route_handlers(&r.route),
    )
}

fn redirect(location: String) -> Value {
    json!({
        "handler": "static_response",
        "status_code": 307,
        "headers": {"Location": [location], "Cache-Control": ["no-store"]}
    })
}

/// The caddy-dns provider object of a domain: `name` and each stored field, secrets as
/// `{file.<dir>/<credential>/<field>}` where the module replaces placeholders, the value
/// itself otherwise.
fn provider_json(input: &RenderInput, d: &WebDomain) -> Value {
    let Some(p) = d.provider else {
        return Value::Null;
    };
    let mut o = serde_json::Map::new();
    o.insert("name".into(), json!(p.as_str()));
    let cred = domains::credential_of(input.cfg, d);
    let stored: Vec<String> = cred.map(|c| c.fields.clone()).unwrap_or_default();
    let values = d.credential.as_ref().and_then(|id| input.creds.get(id));
    for spec in rynas_dns::fields(p) {
        if spec.placeholder {
            if stored.iter().any(|f| f == spec.name) {
                o.insert(
                    spec.name.into(),
                    json!(format!(
                        "{{file.{}/{}/{}}}",
                        input.paths.dns_secrets,
                        d.credential.as_deref().unwrap_or(""),
                        spec.name
                    )),
                );
            }
        } else if let Some(v) = values.and_then(|(_, c)| c.get(spec.name)) {
            o.insert(spec.name.into(), json!(v));
        }
    }
    Value::Object(o)
}

/// The ACME issuer of a domain.
fn issuer(input: &RenderInput, d: &WebDomain) -> Value {
    let mut challenges = json!({"tls-alpn": {"disabled": true}});
    if d.challenge != ChallengeKind::Http01 {
        challenges["http"] = json!({"disabled": true});
        let mut dns = json!({
            "provider": provider_json(input, d),
            "resolvers": PROPAGATION_RESOLVERS,
        });
        if d.challenge == ChallengeKind::CnameDelegation
            && let Some(t) = &d.delegate_to
        {
            dns["override_domain"] = json!(t);
        }
        challenges["dns"] = dns;
    }
    let a = &input.cfg.acme;
    let mut i = json!({
        "module": "acme",
        "ca": domains::acme_ca(input.cfg),
        "challenges": challenges,
    });
    if let Some(e) = &a.email {
        i["email"] = json!(e);
    }
    if let Some(p) = &a.profile {
        i["profile"] = json!(p);
    }
    i
}

/// The `tls` app: local leaf from files, ACME for the own domains. `Null` when neither.
fn tls_app(input: &RenderInput, resolved: &Resolved, used: &[(&Active, Vec<String>)]) -> Value {
    let mut tls = serde_json::Map::new();
    let mut certs = serde_json::Map::new();
    let mut files = Vec::new();
    if !resolved.local_names().is_empty() {
        files.push(json!({
            "certificate": input.paths.cert,
            "key": input.paths.key,
            "tags": [LOCAL_CERT_TAG]
        }));
    }
    for (a, _) in used {
        files.push(json!({"certificate": a.cert, "key": a.key, "tags": [a.tag()]}));
    }
    if !files.is_empty() {
        certs.insert("load_files".into(), Value::Array(files));
    }
    let mut automate: Vec<String> = Vec::new();
    let mut policies = Vec::new();
    if input.cfg.settings.mode == ProxyMode::Builtin {
        for d in domains::active(input.cfg) {
            let subjects = imported::acme_subjects(input.cfg, d, input.imports);
            if subjects.is_empty() {
                continue;
            }
            automate.extend(subjects.iter().cloned());
            policies.push(json!({"subjects": subjects, "issuers": [issuer(input, d)]}));
        }
    }
    if !automate.is_empty() {
        certs.insert("automate".into(), json!(automate));
    }
    if certs.is_empty() {
        return Value::Null;
    }
    tls.insert("certificates".into(), Value::Object(certs));
    if !policies.is_empty() {
        tls.insert("automation".into(), json!({"policies": policies}));
    }
    Value::Object(tls)
}

impl Renderer for Caddy {
    fn render(&self, input: &RenderInput, resolved: &Resolved) -> Value {
        let s = &input.cfg.settings;
        if s.mode == ProxyMode::External {
            return Value::Null;
        }
        let p = input.paths;
        let facts = input.facts;
        let mut https_routes = Vec::new();
        let mut names_all: Vec<String> = Vec::new();
        if !resolved.ui.is_empty() {
            let mut h = baseline_headers(HeaderPreset::Default);
            // the interface streams events (SSE): never buffered, no read timeout
            h.push(proxy(
                dial("127.0.0.1", input.ui_port),
                UpTls::Off,
                None,
                false,
                false,
            ));
            https_routes.push(site(
                "ui",
                &resolved.ui,
                &access_ranges(s.ui.access, facts, &s.ui.allow),
                h,
            ));
            names_all.extend(resolved.ui.iter().cloned());
        }
        // names of routes only on extra ports: `:80` sends them to their first port
        let mut port_only: Vec<(Vec<String>, u16)> = Vec::new();
        for r in &resolved.routes {
            if r.names.is_empty() {
                continue;
            }
            if !r.route.main_port {
                if let Some(p) = r.route.ports.first() {
                    port_only.push((r.names.clone(), *p));
                }
                continue;
            }
            https_routes.push(site_at(
                &format!("route-{}", r.route.id),
                &r.names,
                r.route.prefix.as_deref(),
                &access_ranges(r.route.access, facts, &r.route.allow),
                route_handlers(&r.route),
            ));
            names_all.extend(r.names.iter().cloned());
        }
        // extra ports: the routes on each one, longest path prefix first
        let ports: BTreeSet<u16> = resolved
            .routes
            .iter()
            .filter(|r| r.route.enabled)
            .flat_map(|r| r.route.ports.iter().copied())
            .collect();
        let mut port_servers: Vec<(u16, Vec<Value>)> = Vec::new();
        for &port in &ports {
            let mut on: Vec<&super::ResolvedRoute> = resolved
                .routes
                .iter()
                .filter(|r| r.route.ports.contains(&port) && !r.port_names().is_empty())
                .collect();
            on.sort_by_key(|r| {
                (
                    std::cmp::Reverse(r.route.prefix.as_deref().map_or(0, str::len)),
                    r.route.id.clone(),
                )
            });
            let mut routes: Vec<Value> = on.iter().map(|r| port_site(r, port, input)).collect();
            if routes.is_empty() {
                continue;
            }
            for r in &on {
                for n in r.port_names() {
                    if !names_all.contains(&n) {
                        names_all.push(n);
                    }
                }
            }
            routes.push(json!({"handle": [{"handler": "static_response", "abort": true}]}));
            port_servers.push((port, routes));
        }
        // unknown host names: closed without an answer (no default site)
        https_routes.push(json!({"handle": [{"handler": "static_response", "abort": true}]}));

        let https_suffix = if s.https_port == 443 {
            String::new()
        } else {
            format!(":{}", s.https_port)
        };
        let mut http_routes = Vec::new();
        for (names, port) in &port_only {
            http_routes.push(json!({
                "match": [{"host": names}],
                "handle": [redirect(format!(
                    "https://{{http.request.host}}:{port}{{http.request.uri}}"
                ))],
                "terminal": true
            }));
        }
        let main_names: Vec<String> = resolved
            .ui
            .iter()
            .chain(
                resolved
                    .routes
                    .iter()
                    .filter(|r| r.route.main_port)
                    .flat_map(|r| r.names.iter()),
            )
            .cloned()
            .collect();
        if !main_names.is_empty() {
            http_routes.push(json!({
                "match": [{"host": main_names}],
                "handle": [redirect(format!(
                    "https://{{http.request.host}}{https_suffix}{{http.request.uri}}"
                ))],
                "terminal": true
            }));
        }
        // any other name or a bare address: the interface's own HTTPS port
        http_routes.push(json!({
            "handle": [redirect(format!(
                "https://{{http.request.host}}:{}{{http.request.uri}}",
                input.emergency_port
            ))]
        }));

        let mut servers = serde_json::Map::new();
        servers.insert(
            "rynas_http".into(),
            json!({
                "listen": [format!(":{}", s.http_port)],
                "automatic_https": {"disable": true},
                "routes": http_routes
            }),
        );
        // imported certificates: the names each one serves, in id order
        let owner = imported::assign(input.imports, &names_all);
        let used: Vec<(&Active, Vec<String>)> = input
            .imports
            .iter()
            .map(|a| {
                let mine: Vec<String> = names_all
                    .iter()
                    .filter(|n| owner.get(*n) == Some(&a.id))
                    .cloned()
                    .collect();
                (a, mine)
            })
            .filter(|(_, mine)| !mine.is_empty())
            .collect();
        let mut policies: Vec<Value> = used
            .iter()
            .map(|(a, mine)| {
                json!({
                    "match": {"sni": mine},
                    "certificate_selection": {"any_tag": [a.tag()]}
                })
            })
            .collect();
        policies.push(json!({}));
        let mut apps = serde_json::Map::new();
        if !names_all.is_empty() {
            let protocols: Vec<&str> = if s.h3 {
                vec!["h1", "h2", "h3"]
            } else {
                vec!["h1", "h2"]
            };
            let server = |port: u16, routes: Vec<Value>| {
                json!({
                    "listen": [format!(":{port}")],
                    "protocols": protocols,
                    "strict_sni_host": true,
                    "automatic_https": {"disable": true},
                    "tls_connection_policies": policies,
                    "logs": {},
                    "routes": routes
                })
            };
            servers.insert("rynas_https".into(), server(s.https_port, https_routes));
            for (port, routes) in port_servers {
                servers.insert(format!("rynas_port_{port}"), server(port, routes));
            }
            let tls = tls_app(input, resolved, &used);
            if !tls.is_null() {
                apps.insert("tls".into(), tls);
            }
        }
        apps.insert(
            "http".into(),
            json!({
                "http_port": s.http_port,
                "https_port": s.https_port,
                "grace_period": "10s",
                "servers": servers
            }),
        );
        json!({
            "admin": {
                "listen": format!("unix/{}|0600", p.admin_socket),
                "config": {"persist": false}
            },
            "storage": {"module": "file_system", "root": p.storage},
            "logging": {"logs": {"default": {
                "writer": {"output": "stderr"},
                "encoder": {"format": "json"}
            }}},
            "apps": apps
        })
    }
}
