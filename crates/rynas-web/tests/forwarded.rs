//! Behind the built-in reverse proxy (D-PROXY, wave W1b): `X-Forwarded-For` trusted only from
//! the proxy on loopback over the plain port, the login rate limit per real client, the
//! session cookie and CSRF through the proxy, and `--listen auto` moving the plain port.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use common::TestApp;
use rynas_api_types::{Session, error_codes};
use rynas_web::conn::ConnAddrs;
use rynas_web::hardware::NoProvider;
use rynas_web::listen::{Plain, PlainListen};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

/// How a request reaches rynas-web.
#[derive(Clone, Copy)]
enum Via {
    /// rynas-caddy: loopback peer on the plain port's loopback address.
    Proxy,
    /// A host on the network, straight to the plain port.
    Lan,
    /// The HTTPS port, from loopback.
    HttpsLoopback,
}

struct Reply {
    status: StatusCode,
    cookie: Option<String>,
    body: Vec<u8>,
}

async fn send(
    app: &TestApp,
    via: Via,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Reply {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string())),
        None => b.body(Body::empty()),
    }
    .unwrap();
    let (remote, local) = match via {
        Via::Proxy => ("127.0.0.1:51000", "127.0.0.1:8080"),
        Via::Lan => ("192.0.2.50:40000", "192.0.2.1:8080"),
        Via::HttpsLoopback => ("127.0.0.1:51001", "127.0.0.1:8443"),
    };
    req.extensions_mut().insert(ConnectInfo(ConnAddrs {
        remote: remote.parse().unwrap(),
        local: Some(local.parse().unwrap()),
    }));
    if matches!(via, Via::HttpsLoopback) {
        req.extensions_mut().insert(rynas_web::tls::Https);
    }
    let r = app.router.clone().oneshot(req).await.unwrap();
    let (parts, body) = r.into_parts();
    Reply {
        status: parts.status,
        cookie: parts
            .headers
            .get(header::SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_string()),
        body: to_bytes(body, 1 << 20).await.unwrap().to_vec(),
    }
}

async fn bad_login(app: &TestApp, via: Via, user: &str, xff: &str) -> StatusCode {
    send(
        app,
        via,
        "POST",
        "/api/v1/auth/login",
        &[("x-forwarded-for", xff)],
        Some(json!({"username": user, "password": "wrong"})),
    )
    .await
    .status
}

/// Failed logins with fresh user names (so only the per-address limit counts) until the
/// address is throttled; the number of attempts it took.
async fn exhaust(app: &TestApp, via: Via, xff: &dyn Fn(usize) -> String) -> usize {
    for i in 0..40 {
        let s = bad_login(app, via, &format!("user{i}"), &xff(i)).await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            return i;
        }
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    panic!("never throttled");
}

#[tokio::test]
async fn rate_limit_per_real_client_behind_the_proxy() {
    let app = common::app(Arc::new(NoProvider));
    // one client behind the proxy is throttled...
    let n = exhaust(&app, Via::Proxy, &|_| "192.0.2.10".into()).await;
    assert!(n > 1);
    // ...another one is not, though both reach rynas-web from 127.0.0.1
    assert_eq!(
        bad_login(&app, Via::Proxy, "fresh", "192.0.2.11").await,
        StatusCode::UNAUTHORIZED
    );
    // a client cannot escape by prepending addresses: the right-most entry counts
    assert_eq!(
        bad_login(&app, Via::Proxy, "fresh2", "203.0.113.1, 192.0.2.10").await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn spoofed_headers_from_the_network_are_ignored() {
    let app = common::app(Arc::new(NoProvider));
    // a LAN host rotating X-Forwarded-For is still one client
    let n = exhaust(&app, Via::Lan, &|i| format!("198.51.100.{i}")).await;
    let m = exhaust(&common::app(Arc::new(NoProvider)), Via::Lan, &|_| {
        "x".into()
    })
    .await;
    assert_eq!(n, m, "the header made no difference");
    // and it cannot get another client throttled either
    assert_eq!(
        bad_login(&app, Via::Proxy, "fresh", "198.51.100.1").await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_https_port_never_trusts_the_header() {
    let app = common::app(Arc::new(NoProvider));
    // from loopback over HTTPS every rotating header lands on 127.0.0.1...
    exhaust(&app, Via::HttpsLoopback, &|i| format!("198.51.100.{i}")).await;
    // ...which is the proxy's own address without a header: throttled too
    assert_eq!(
        send(
            &app,
            Via::Proxy,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(json!({"username": "fresh", "password": "wrong"})),
        )
        .await
        .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    // a proxied client is a different address
    assert_eq!(
        bad_login(&app, Via::Proxy, "fresh", "192.0.2.10").await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn cookie_and_csrf_through_the_proxy() {
    let app = common::app(Arc::new(NoProvider));
    // what Caddy sends: the browser's Host (the interface's name), X-Forwarded-*
    let proxied = [
        ("host", "nas.local"),
        ("origin", "https://nas.local"),
        ("x-forwarded-for", "192.0.2.10"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-host", "nas.local"),
    ];
    let r = send(
        &app,
        Via::Proxy,
        "POST",
        "/api/v1/auth/login",
        &proxied,
        Some(json!({"username": "admin", "password": "admin-pw"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let set = r.cookie.unwrap();
    assert!(set.ends_with("; Secure"), "{set}");
    assert!(
        set.contains("SameSite=Strict") && set.contains("HttpOnly"),
        "{set}"
    );
    let session: Session = serde_json::from_slice(&r.body).unwrap();
    let token = set
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("rynas_session=")
        .unwrap()
        .to_string();
    let cookie = format!("rynas_session={token}");

    let mut with_cookie = proxied.to_vec();
    with_cookie.push(("cookie", &cookie));
    // reads work on the proxy's name
    let r = send(
        &app,
        Via::Proxy,
        "GET",
        "/api/v1/auth/session",
        &with_cookie,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    // a change without the CSRF token is refused through the proxy as well
    let r = send(
        &app,
        Via::Proxy,
        "POST",
        "/api/v1/auth/logout",
        &with_cookie,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let e: rynas_api_types::ApiError = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(e.code, error_codes::CSRF);
    // an Origin of another site does not help a forged request without the token, and the
    // token works whatever the Host (it is bound to the session, not to a name)
    let mut forged = with_cookie.clone();
    forged.retain(|(k, _)| *k != "origin");
    forged.push(("origin", "https://evil.example"));
    let r = send(
        &app,
        Via::Proxy,
        "POST",
        "/api/v1/auth/logout",
        &forged,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let mut good = with_cookie.clone();
    good.push(("x-csrf-token", &session.csrf_token));
    let r = send(&app, Via::Proxy, "POST", "/api/v1/auth/logout", &good, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    // the cookie is cleared with the same attributes
    assert!(r.cookie.unwrap().ends_with("; Secure"));

    // plain HTTP from the network without the header: no Secure flag (the browser would drop
    // the cookie)
    let r = send(
        &app,
        Via::Lan,
        "POST",
        "/api/v1/auth/login",
        &[("host", "nas.local:8080")],
        Some(json!({"username": "admin", "password": "admin-pw"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.cookie.unwrap().contains("Secure"));
}

// ---------------------------------------------------------------------------------------------
// real sockets
// ---------------------------------------------------------------------------------------------

/// One HTTP/1.1 request to `addr`; the status code.
async fn http(addr: SocketAddr, req: &str) -> u16 {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    text.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn login_request(user: &str, xff: &str) -> String {
    let body = json!({"username": user, "password": "wrong"}).to_string();
    format!(
        "POST /api/v1/auth/login HTTP/1.1\r\nHost: nas.local\r\nX-Forwarded-For: {xff}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn plain(app: &TestApp, mode: PlainListen, state: &std::path::Path, ready: bool) -> Plain {
    let flag = Arc::new(AtomicBool::new(ready));
    Plain {
        mode,
        app: app.router.clone(),
        proxy_state: state.to_path_buf(),
        https_ready: Arc::new(move || flag.load(Ordering::SeqCst)),
        check: Duration::from_millis(50),
    }
}

#[tokio::test]
async fn loopback_listener_trusts_the_proxy_end_to_end() {
    let app = common::app(Arc::new(NoProvider));
    let d = tempfile::tempdir().unwrap();
    let p = plain(
        &app,
        PlainListen::Fixed("127.0.0.1:0".parse().unwrap()),
        &d.path().join("none"),
        true,
    );
    let (addr, serve) = p
        .bind(std::future::pending(), Duration::ZERO)
        .await
        .unwrap();
    tokio::spawn(serve);
    let mut throttled = false;
    for i in 0..40 {
        match http(addr, &login_request(&format!("u{i}"), "192.0.2.10")).await {
            429 => {
                throttled = true;
                break;
            }
            s => assert_eq!(s, 401),
        }
    }
    assert!(throttled);
    // another client behind the proxy
    assert_eq!(http(addr, &login_request("fresh", "192.0.2.11")).await, 401);
}

/// Whether something listens on every address of `port` (a wildcard socket): binding a
/// second loopback address of the same port then fails.
fn wildcard_taken(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.2", port)).is_err()
}

async fn wait_for(mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn auto_moves_the_plain_port_to_loopback_and_back() {
    let app = common::app(Arc::new(NoProvider));
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("web-access.ui");
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let lo: SocketAddr = ([127, 0, 0, 1], port).into();
    let session = "GET /api/v1/auth/session HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

    let (addr, serve) = plain(&app, PlainListen::Auto(port), &state, true)
        .bind(std::future::pending(), Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(addr, ([0, 0, 0, 0], port).into());
    tokio::spawn(serve);
    assert!(wildcard_taken(port));
    assert_eq!(http(lo, session).await, 401);

    // rynasd says the proxy fronts the interface: loopback only, still answering there
    std::fs::write(&state, "nas.local\nnas\n").unwrap();
    wait_for(|| !wildcard_taken(port)).await;
    assert_eq!(http(lo, session).await, 401);

    // external mode (the file goes away): back on the network
    std::fs::remove_file(&state).unwrap();
    wait_for(|| wildcard_taken(port)).await;
    assert_eq!(http(lo, session).await, 401);
}

#[tokio::test]
async fn auto_keeps_the_network_without_https() {
    let app = common::app(Arc::new(NoProvider));
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("web-access.ui");
    std::fs::write(&state, "nas.local\n").unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    // the proxy fronts the interface, but the emergency door is not up: stay reachable
    let (addr, serve) = plain(&app, PlainListen::Auto(port), &state, false)
        .bind(std::future::pending(), Duration::ZERO)
        .await
        .unwrap();
    assert!(addr.ip().is_unspecified());
    tokio::spawn(serve);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(wildcard_taken(port));
}
