//! HTTPS on its own port: a certificate from a local CA (as rynasd makes it), a real TLS
//! client that trusts only that CA, hot reload of the pair, the plain-HTTP redirect.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use rynas_tls::PemPair;
use rynas_web::hardware::NoProvider;
use rynas_web::tls::{CertReloader, Redirect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

fn write_pair(dir: &Path, p: &PemPair) {
    std::fs::write(dir.join("ui.key"), &p.key).unwrap();
    std::fs::write(dir.join("ui.crt"), &p.chain).unwrap();
}

fn local_pair() -> (PemPair, PemPair) {
    let now = rynas_tls::now();
    let ca = rynas_tls::generate_ca("testnas", now).unwrap();
    let leaf = rynas_tls::issue_leaf(&ca, &rynas_tls::names("testnas", &[]), now).unwrap();
    (ca, leaf)
}

/// One HTTP/1.1 request over TLS to 127.0.0.1:`port`, trusting only `ca`; the raw answer.
async fn https(
    port: u16,
    ca: &PemPair,
    server_name: &str,
    request: &str,
) -> Result<String, String> {
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca.chain.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = ServerName::try_from(server_name.to_string()).unwrap();
    let mut tls = connector
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

async fn start(dir: &Path) -> (u16, Arc<CertReloader>, common::TestApp) {
    let reloader = CertReloader::new(dir.join("ui.crt"), dir.join("ui.key"));
    let app = common::app(Arc::new(NoProvider));
    let listener = rynas_web::tls::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(rynas_web::tls::serve(
        listener,
        app.router.clone(),
        reloader.server_config(),
    ));
    (port, reloader, app)
}

const SESSION: &str =
    "GET /api/v1/auth/session HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

#[tokio::test]
async fn serves_https_with_the_local_ca_and_secure_cookies() {
    let d = tempfile::tempdir().unwrap();
    let (ca, leaf) = local_pair();
    write_pair(d.path(), &leaf);
    let (port, reloader, _app) = start(d.path()).await;
    assert!(!reloader.ready());
    // no certificate loaded yet: the handshake fails
    assert!(https(port, &ca, "localhost", SESSION).await.is_err());
    assert!(reloader.reload().unwrap());
    assert!(reloader.ready());
    // nothing changed: no reload
    assert!(!reloader.reload().unwrap());

    let r = https(port, &ca, "localhost", SESSION).await.unwrap();
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    // every name of the leaf works, the IP too
    for name in ["testnas", "testnas.local", "127.0.0.1"] {
        let r = https(port, &ca, name, SESSION).await.unwrap();
        assert!(r.starts_with("HTTP/1.1 401"), "{name}: {r}");
    }
    // a name not in the certificate is refused by the client
    assert!(https(port, &ca, "other.example", SESSION).await.is_err());

    // login over HTTPS: the cookie is Secure, and no HSTS on the emergency port
    let body = r#"{"username":"admin","password":"admin-pw"}"#;
    let req = format!(
        "POST /api/v1/auth/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let r = https(port, &ca, "localhost", &req).await.unwrap();
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    let lower = r.to_ascii_lowercase();
    let cookie = lower
        .lines()
        .find(|l| l.starts_with("set-cookie:"))
        .unwrap();
    assert!(cookie.contains("; secure"), "{cookie}");
    assert!(cookie.contains("httponly") && cookie.contains("samesite=strict"));
    assert!(!lower.contains("strict-transport-security"));
}

#[tokio::test]
async fn reloads_a_new_pair_and_ignores_a_mismatched_one() {
    let d = tempfile::tempdir().unwrap();
    let (ca1, leaf1) = local_pair();
    write_pair(d.path(), &leaf1);
    let (port, reloader, _app) = start(d.path()).await;
    reloader.reload().unwrap();
    assert!(https(port, &ca1, "localhost", SESSION).await.is_ok());

    // a pair from another CA replaces it without a restart
    let (ca2, leaf2) = local_pair();
    write_pair(d.path(), &leaf2);
    assert!(reloader.reload().unwrap());
    assert!(https(port, &ca1, "localhost", SESSION).await.is_err());
    assert!(https(port, &ca2, "localhost", SESSION).await.is_ok());

    // a chain whose key is still the old one (an update half done) is not taken
    let (_, leaf3) = local_pair();
    std::fs::write(d.path().join("ui.crt"), &leaf3.chain).unwrap();
    assert!(
        reloader
            .reload_now()
            .unwrap_err()
            .contains("does not match")
    );
    assert!(https(port, &ca2, "localhost", SESSION).await.is_ok());
    // missing files: the previous pair stays
    std::fs::remove_file(d.path().join("ui.key")).unwrap();
    assert!(reloader.reload().is_err());
    assert!(reloader.ready());
}

async fn plain(
    redirect: &axum::Router,
    peer: &str,
    host: &str,
    xfp: Option<&str>,
) -> axum::response::Response {
    let mut req = Request::builder()
        .uri("/storage?tab=disks")
        .header(header::HOST, host);
    if let Some(x) = xfp {
        req = req.header("x-forwarded-proto", x);
    }
    let mut req = req.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    redirect.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn plain_http_redirects_once_https_is_ready_except_loopback() {
    let d = tempfile::tempdir().unwrap();
    let (_, leaf) = local_pair();
    let reloader = CertReloader::new(d.path().join("ui.crt"), d.path().join("ui.key"));
    let app = common::app(Arc::new(NoProvider));
    let cfg = Arc::new(Redirect {
        port: 8443,
        reloader: reloader.clone(),
    });
    let router = app
        .router
        .clone()
        .layer(axum::middleware::from_fn_with_state(
            cfg,
            rynas_web::tls::redirect,
        ));

    // no certificate yet: served as is
    let r = plain(&router, "192.0.2.5:40000", "nas.local:8080", None).await;
    assert_ne!(r.status(), StatusCode::TEMPORARY_REDIRECT);

    write_pair(d.path(), &leaf);
    reloader.reload().unwrap();
    let r = plain(&router, "192.0.2.5:40000", "nas.local:8080", None).await;
    assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        r.headers()[header::LOCATION],
        "https://nas.local:8443/storage?tab=disks"
    );
    assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    // IPv4-mapped peer, IPv6 host
    let r = plain(
        &router,
        "[::ffff:192.0.2.5]:40000",
        "[2001:db8::5]:8080",
        None,
    )
    .await;
    assert_eq!(
        r.headers()[header::LOCATION],
        "https://[2001:db8::5]:8443/storage?tab=disks"
    );

    // loopback (a reverse proxy on this host, an SSH tunnel), a proxy that terminated TLS,
    // an odd Host: never redirected
    for (peer, host, xfp) in [
        ("127.0.0.1:40000", "nas.local:8080", None),
        ("[::1]:40000", "nas.local:8080", None),
        ("[::ffff:127.0.0.1]:40000", "nas.local:8080", None),
        ("192.0.2.5:40000", "nas.local", Some("https")),
        ("192.0.2.5:40000", "evil/x", None),
    ] {
        let r = plain(&router, peer, host, xfp).await;
        assert_ne!(r.status(), StatusCode::TEMPORARY_REDIRECT, "{peer} {host}");
    }
}
