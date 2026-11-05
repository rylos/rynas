//! Login, sessions, CSRF, rate limiting and TOTP through the HTTP API (fake password backend).

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode, header};
use common::{Creds, DownAuth, TestApp, app, build, cookie_value};
use futures_util::StreamExt;
use rynas_api_types::{
    LoginRequest, MetricsSample, Role, Session, TotpConfirmed, TotpEnrollment, error_codes,
};
use rynas_auth::totp::code_at;
use rynas_web::hardware::NoProvider;
use serde_json::json;
use tower::ServiceExt;

fn login_req(user: &str, pw: &str, totp: Option<&str>) -> LoginRequest {
    LoginRequest {
        username: user.into(),
        password: pw.into(),
        totp: totp.map(Into::into),
    }
}

fn now_code(app: &TestApp, secret: &str) -> String {
    use rynas_auth::Clock;
    code_at(secret, app.clock.now_ms() / 1000).unwrap()
}

#[tokio::test]
async fn api_requires_a_session() {
    let app = app(Arc::new(NoProvider));
    let bogus = Creds {
        cookie: Some("bogus".into()),
        csrf: None,
    };
    for uri in [
        "/api/v1/system",
        "/api/v1/storage",
        "/api/v1/metrics/stream",
        "/api/v1/nope",
        "/api/v1/auth/session",
    ] {
        for c in [&Creds::default(), &bogus] {
            let r = app.get(uri, c).await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{uri}");
            assert_eq!(r.code(), error_codes::UNAUTHENTICATED);
        }
    }
    let r = app
        .send(Method::POST, "/api/v1/auth/logout", &Creds::default(), None)
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_session_logout() {
    let app = app(Arc::new(NoProvider));
    let r = app.try_login(&login_req("admin", "admin-pw", None)).await;
    assert_eq!(r.status, StatusCode::OK);
    let set_cookie = r.headers[header::SET_COOKIE].to_str().unwrap().to_owned();
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=43200"] {
        assert!(set_cookie.contains(attr), "{set_cookie}");
    }
    assert!(!set_cookie.contains("Secure"), "plain HTTP: {set_cookie}");
    assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");
    let s: Session = r.json();
    assert_eq!(s.user.username, "admin");
    assert_eq!(s.user.realm, "pam");
    assert_eq!(s.user.role, Role::Admin);
    assert!(!s.totp_enabled);
    assert_eq!(s.expires_ms, common::T0_MS + 12 * 3600 * 1000);
    assert_eq!(s.idle_timeout_s, 1800);
    let token = cookie_value(&r.headers).unwrap();
    assert_eq!(token.len(), 43);
    let c = Creds {
        cookie: Some(token),
        csrf: Some(s.csrf_token.clone()),
    };

    let r = app.get("/api/v1/auth/session", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<Session>().csrf_token, s.csrf_token);
    assert_eq!(app.get("/api/v1/system", &c).await.status, StatusCode::OK);

    // logout is state-changing: CSRF token required
    let no_csrf = Creds {
        csrf: None,
        ..c.clone()
    };
    let wrong = Creds {
        csrf: Some("x".repeat(43)),
        ..c.clone()
    };
    for bad in [&no_csrf, &wrong] {
        let r = app
            .send(Method::POST, "/api/v1/auth/logout", bad, None)
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        assert_eq!(r.code(), error_codes::CSRF);
    }
    let r = app
        .send(Method::POST, "/api/v1/auth/logout", &c, None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let cleared = r.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cleared.starts_with("rynas_session=;") && cleared.contains("Max-Age=0"));
    assert_eq!(
        app.get("/api/v1/auth/session", &c).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.get("/api/v1/system", &c).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn secure_cookie_behind_https_proxy() {
    let app = app(Arc::new(NoProvider));
    let req = axum::http::Request::post("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-forwarded-proto", "https")
        .body(axum::body::Body::from(
            json!({"username": "admin", "password": "admin-pw"}).to_string(),
        ))
        .unwrap();
    let r = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(
        r.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .ends_with("; Secure")
    );
}

#[tokio::test]
async fn bad_logins_look_the_same() {
    let app = app(Arc::new(NoProvider));
    for (u, p) in [
        ("admin", "wrong"),
        ("nobody", "admin-pw"),
        ("guest", "guest-pw"), // right password, not an administrator
        ("../etc/passwd", "x"),
        ("admin", ""),
    ] {
        let r = app.try_login(&login_req(u, p, None)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{u}");
        assert_eq!(r.code(), error_codes::INVALID_CREDENTIALS, "{u}");
        assert!(r.headers.get(header::SET_COOKIE).is_none());
    }
    // not JSON
    let r = app
        .router
        .clone()
        .oneshot(
            axum::http::Request::post("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(axum::body::Body::from("username=admin&password=admin-pw"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn rate_limit_with_retry_after() {
    let app = app(Arc::new(NoProvider));
    for _ in 0..4 {
        let r = app.try_login(&login_req("admin", "wrong", None)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    // 3 free failures, the 4th locks the user for 4 s: even the right password waits
    let r = app.try_login(&login_req("admin", "admin-pw", None)).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.code(), error_codes::RATE_LIMITED);
    assert_eq!(r.headers[header::RETRY_AFTER], "4");
    app.clock.advance_ms(3_500);
    let r = app.try_login(&login_req("admin", "admin-pw", None)).await;
    assert_eq!(r.headers[header::RETRY_AFTER], "1", "rounded up");
    app.clock.advance_ms(500);
    app.login().await;
    // a success resets the user counter
    app.try_login(&login_req("admin", "wrong", None)).await;
    app.login().await;
}

#[tokio::test]
async fn idle_and_absolute_expiry() {
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    app.clock.advance_ms(29 * 60 * 1000);
    assert_eq!(app.get("/api/v1/system", &c).await.status, StatusCode::OK);
    app.clock.advance_ms(29 * 60 * 1000);
    assert_eq!(
        app.get("/api/v1/auth/session", &c).await.status,
        StatusCode::OK
    );
    app.clock.advance_ms(31 * 60 * 1000);
    assert_eq!(
        app.get("/api/v1/system", &c).await.status,
        StatusCode::UNAUTHORIZED
    );

    let c = app.login().await;
    for _ in 0..(12 * 3) {
        app.clock.advance_ms(20 * 60 * 1000);
        let _ = app.get("/api/v1/auth/session", &c).await;
    }
    assert_eq!(
        app.get("/api/v1/system", &c).await.status,
        StatusCode::UNAUTHORIZED
    );
}

/// GET with the background marker of the UI's timers (`X-Rynas-Background: 1`).
async fn get_bg(app: &TestApp, uri: &str, c: &Creds) -> StatusCode {
    let req = axum::http::Request::get(uri)
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.clone().unwrap()),
        )
        .header(rynas_api_types::BACKGROUND_HEADER, "1")
        .body(axum::body::Body::empty())
        .unwrap();
    app.router.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn background_requests_do_not_renew_the_idle_timeout() {
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    // a tab left open: its pollers run every few seconds, for longer than the idle timeout
    for _ in 0..29 {
        app.clock.advance_ms(60 * 1000);
        assert_eq!(get_bg(&app, "/api/v1/hardware", &c).await, StatusCode::OK);
        assert_eq!(get_bg(&app, "/api/v1/system", &c).await, StatusCode::OK);
        assert_eq!(
            get_bg(&app, "/api/v1/auth/session", &c).await,
            StatusCode::OK
        );
    }
    app.clock.advance_ms(2 * 60 * 1000);
    assert_eq!(
        get_bg(&app, "/api/v1/hardware", &c).await,
        StatusCode::UNAUTHORIZED,
        "31 minutes without a user action"
    );
    assert_eq!(
        app.get("/api/v1/system", &c).await.status,
        StatusCode::UNAUTHORIZED
    );

    // a user action in between renews it; background requests after it do not
    let c = app.login().await;
    app.clock.advance_ms(20 * 60 * 1000);
    assert_eq!(app.get("/api/v1/system", &c).await.status, StatusCode::OK);
    app.clock.advance_ms(20 * 60 * 1000);
    assert_eq!(get_bg(&app, "/api/v1/system", &c).await, StatusCode::OK);
    app.clock.advance_ms(11 * 60 * 1000);
    assert_eq!(
        get_bg(&app, "/api/v1/system", &c).await,
        StatusCode::UNAUTHORIZED
    );

    // an open event stream does not keep the session alive either
    let c = app.login().await;
    app.clock.advance_ms(29 * 60 * 1000);
    assert_eq!(get_bg(&app, "/api/v1/events", &c).await, StatusCode::OK);
    app.clock.advance_ms(2 * 60 * 1000);
    assert_eq!(
        app.get("/api/v1/system", &c).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn events_carry_hardware_changes() {
    use rynas_api_types::HardwareStatus;
    let app = app(Arc::new(NoProvider));
    let hw = |duty: u8, age: u64| HardwareStatus {
        provider: Some("synology".into()),
        fan_duty: Some(duty),
        age_s: Some(age),
        ..Default::default()
    };
    assert!(app.store.push_hardware(hw(30, 1)));
    // only the age changed: not sent again
    assert!(!app.store.push_hardware(hw(30, 2)));
    let c = app.login().await;
    let req = axum::http::Request::get("/api/v1/events")
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.clone().unwrap()),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let r = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body().into_data_stream();
    let mut text = String::new();
    let mut read_until = async |what: &str| {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !text.contains(what) {
                match body.next().await {
                    Some(Ok(chunk)) => text.push_str(&String::from_utf8_lossy(&chunk)),
                    _ => return false,
                }
            }
            true
        })
        .await
        .unwrap_or(false)
    };
    // the latest status on connect, then each change
    assert!(read_until("\"fan_duty\":30").await);
    assert!(app.store.push_hardware(hw(55, 3)));
    assert!(read_until("\"fan_duty\":55").await);
    assert!(text.contains("event: hardware.status"));
}

#[tokio::test]
async fn sse_ends_with_the_session() {
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    let req = axum::http::Request::get("/api/v1/metrics/stream")
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.clone().unwrap()),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let r = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body().into_data_stream();
    let first = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("event: sample"));

    app.store.push(MetricsSample {
        ts_ms: 2_000_000,
        ..Default::default()
    });
    assert!(body.next().await.is_some(), "still logged in");
    let r = app
        .send(Method::POST, "/api/v1/auth/logout", &c, None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    app.store.push(MetricsSample {
        ts_ms: 2_001_000,
        ..Default::default()
    });
    let end = tokio::time::timeout(Duration::from_secs(5), body.next()).await;
    assert!(matches!(end, Ok(None)), "stream closed after logout");
}

#[tokio::test]
async fn event_streams_are_capped_per_session() {
    use rynas_web::streams::PER_SESSION;
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    let open = async |uri: &str| {
        let req = axum::http::Request::get(uri)
            .header(
                header::COOKIE,
                format!("rynas_session={}", c.cookie.clone().unwrap()),
            )
            .body(axum::body::Body::empty())
            .unwrap();
        let r = app.router.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.into_body().into_data_stream()
    };
    let mut first = open("/api/v1/events").await;
    let mut rest = Vec::new();
    for i in 1..PER_SESSION {
        let uri = if i % 2 == 0 {
            "/api/v1/events"
        } else {
            "/api/v1/metrics/stream"
        };
        rest.push(open(uri).await);
    }
    assert_eq!(app.streams.open(), PER_SESSION);
    // one more: the oldest stream of the session ends, the new one is served
    let _newest = open("/api/v1/events").await;
    assert_eq!(app.streams.open(), PER_SESSION);
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(_)) = first.next().await {}
    })
    .await;
    assert!(ended.is_ok(), "the oldest stream was closed");
    // the streams give their place back when the client goes away
    drop(first);
    drop(rest);
    assert_eq!(app.streams.open(), 1);
    // another session is not affected by this one's cap
    let other = app.login().await;
    assert_eq!(
        app.get("/api/v1/system", &other).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn slow_sse_client_gets_resync() {
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    let req = axum::http::Request::get("/api/v1/events")
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.clone().unwrap()),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let r = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // more samples than the channel holds while the client does not read
    for i in 0..40 {
        app.store.push(MetricsSample {
            ts_ms: 3_000_000 + i,
            ..Default::default()
        });
    }
    let mut body = r.into_body().into_data_stream();
    let mut text = String::new();
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(chunk)) = body.next().await {
            text.push_str(&String::from_utf8_lossy(&chunk));
            if text.contains("event: resync") {
                return true;
            }
        }
        false
    })
    .await;
    assert!(matches!(got, Ok(true)), "{text}");
    assert!(text.contains("data: {\"stream\":\"metrics\"}"), "{text}");
}

#[tokio::test]
async fn totp_lifecycle() {
    let app = app(Arc::new(NoProvider));
    let other = app.login().await;
    let c = app.login().await;

    // CSRF applies to the TOTP endpoints too
    let no_csrf = Creds {
        csrf: None,
        ..c.clone()
    };
    let r = app
        .send(Method::POST, "/api/v1/auth/totp/enroll", &no_csrf, None)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let r = app
        .send(
            Method::POST,
            "/api/v1/auth/totp/confirm",
            &c,
            Some(json!({"code": "123456"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.code(), error_codes::TOTP_NOT_ENROLLED);

    let r = app
        .send(Method::POST, "/api/v1/auth/totp/enroll", &c, None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let e: TotpEnrollment = r.json();
    assert_eq!(e.account, "admin@testnas");
    assert!(
        e.otpauth_uri
            .starts_with("otpauth://totp/rynas:admin%40testnas?secret=")
    );
    assert!(e.otpauth_uri.contains(&e.secret));
    assert_eq!((e.digits, e.period_s), (6, 30));

    let r = app
        .send(
            Method::POST,
            "/api/v1/auth/totp/confirm",
            &c,
            Some(json!({"code": "000000"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), error_codes::INVALID_TOTP);

    let code = now_code(&app, &e.secret);
    let r = app
        .send(
            Method::POST,
            "/api/v1/auth/totp/confirm",
            &c,
            Some(json!({ "code": code })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let recovery = r.json::<TotpConfirmed>().recovery_codes;
    assert_eq!(recovery.len(), 10);
    assert!(
        app.get("/api/v1/auth/session", &c)
            .await
            .json::<Session>()
            .totp_enabled
    );
    assert_eq!(
        app.get("/api/v1/auth/session", &other).await.status,
        StatusCode::UNAUTHORIZED,
        "sessions opened without the second factor end"
    );
    let r = app
        .send(Method::POST, "/api/v1/auth/totp/enroll", &c, None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.code(), error_codes::TOTP_ALREADY_ENABLED);

    // login now needs the second factor
    let r = app.try_login(&login_req("admin", "admin-pw", None)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.code(), error_codes::TOTP_REQUIRED);
    let r = app.try_login(&login_req("admin", "wrong", None)).await;
    assert_eq!(
        r.code(),
        error_codes::INVALID_CREDENTIALS,
        "password checked first"
    );
    let r = app
        .try_login(&login_req("admin", "admin-pw", Some(&code)))
        .await;
    assert_eq!(
        r.code(),
        error_codes::INVALID_TOTP,
        "the confirm code cannot be replayed"
    );
    app.clock.advance_ms(30_000);
    let code = now_code(&app, &e.secret);
    let s: Session = app
        .try_login(&login_req("admin", "admin-pw", Some(&code)))
        .await
        .json();
    assert!(s.totp_enabled);

    // recovery codes work once
    let rc = &recovery[0];
    assert_eq!(
        app.try_login(&login_req("admin", "admin-pw", Some(rc)))
            .await
            .status,
        StatusCode::OK
    );
    let r = app
        .try_login(&login_req("admin", "admin-pw", Some(rc)))
        .await;
    assert_eq!(r.code(), error_codes::INVALID_TOTP);

    // disable needs a valid code
    let c = app.login_as("admin", "admin-pw", Some(&recovery[1])).await;
    let r = app
        .send(
            Method::POST,
            "/api/v1/auth/totp/disable",
            &c,
            Some(json!({"code": "111111"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .send(
            Method::POST,
            "/api/v1/auth/totp/disable",
            &c,
            Some(json!({"code": recovery[2]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(
        !app.get("/api/v1/auth/session", &c)
            .await
            .json::<Session>()
            .totp_enabled
    );
    app.login().await;
}

#[tokio::test]
async fn backend_down_is_503() {
    let app = build(Arc::new(NoProvider), Arc::new(DownAuth));
    let r = app.try_login(&login_req("admin", "admin-pw", None)).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.code(), error_codes::AUTH_UNAVAILABLE);
}
