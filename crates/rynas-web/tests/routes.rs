//! Every rynasd route a browser needs is reachable through rynas-web.
//!
//! rynas-web forwards rynasd's endpoints one by one (`proxy::routes` and the few handlers
//! that add something of their own): a route added to rynasd and forgotten there answers 404
//! to the UI. This test reads rynasd's route table from its sources, sends each route and
//! method through the full rynas-web router as a logged-in administrator, and checks that a
//! fake rynasd behind the socket received exactly that request. Routes only rynas-web itself,
//! `rynasctl` or the systemd units call are listed in [`INTERNAL`] with the reason.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::http::Method;
use common::{FakeAuth, build_with_rynasd};
use rynas_web::hardware::NoProvider;

/// rynasd routes that are not part of the browser API, with who calls them.
const INTERNAL: &[(&str, &str)] = &[
    ("/v1/health", "rynas-web's own readiness check"),
    ("/v1/authenticate", "rynas-web's login (PAM behind rynasd)"),
    ("/v1/users/{user}/groups", "rynas-web's session refresh"),
    ("/v1/totp/{user}", "rynas-web's login and /auth/totp/*"),
    ("/v1/totp/{user}/enroll", "rynas-web's /auth/totp/enroll"),
    ("/v1/totp/{user}/confirm", "rynas-web's /auth/totp/confirm"),
    ("/v1/totp/{user}/disable", "rynas-web's /auth/totp/disable"),
    ("/v1/totp/{user}/verify", "rynas-web's login"),
    ("/v1/notifications/events", "rynas-web emits hardware and login events"),
    ("/v1/notifications/stream", "rynas-web multiplexes it into /api/v1/events"),
    ("/v1/notify", "the hardware modules' own socket, not rynasd's API"),
    ("/v1/network/events/flush", "rynasctl, root only"),
    ("/v1/power/schedule/rules/{rule}/shutdown", "rynasctl from the power timers, root only"),
    ("/v1/scheduler/tasks/{id}/finished", "rynasctl from the task units, root only"),
    ("/v1/scheduler/tasks/{id}/execute", "rynasctl from the task units, root only"),
    ("/v1/crypt/auto-unlock", "rynas-crypt-unlock.service (rynasctl), root only"),
    ("/v1/ups/safe-mode", "rynasctl from the UPS hooks, root only"),
];

/// One `.route(...)` of rynasd: its path and the methods it answers.
#[derive(Debug)]
struct Route {
    file: PathBuf,
    path: String,
    methods: Vec<Method>,
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The text of the call starting right after `.route(`, up to its closing parenthesis.
fn call_args(s: &str) -> &str {
    let mut depth = 1usize;
    let mut in_str = false;
    let mut prev = '\0';
    for (i, c) in s.char_indices() {
        match c {
            '"' if prev != '\\' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return &s[..i];
                }
            }
            _ => {}
        }
        prev = c;
    }
    s
}

fn methods_of(args: &str) -> Vec<Method> {
    let mut out = Vec::new();
    for (name, m) in [
        ("get", Method::GET),
        ("post", Method::POST),
        ("put", Method::PUT),
        ("delete", Method::DELETE),
        ("patch", Method::PATCH),
        // a catch-all: the browser reaches it at least with GET
        ("any", Method::GET),
    ] {
        let found = args.match_indices(&format!("{name}(")).any(|(i, _)| {
            let before = args[..i].chars().next_back();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
        });
        if found && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// rynasd's routes, read from the `.route("/v1/…", …)` calls in its sources (tests left out).
fn rynasd_routes() -> Vec<Route> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rynasd/src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        // unit tests at the bottom of a file may build routers of their own
        let text = text.split("#[cfg(test)]").next().unwrap_or("");
        for (i, _) in text.match_indices(".route(") {
            let args = call_args(&text[i + ".route(".len()..]);
            let Some(start) = args.find('"') else { continue };
            let rest = &args[start + 1..];
            let Some(end) = rest.find('"') else { continue };
            let path = &rest[..end];
            if !path.starts_with("/v1/") {
                continue;
            }
            out.push(Route {
                file: f.strip_prefix(&src).unwrap().to_path_buf(),
                path: path.to_string(),
                methods: methods_of(&rest[end + 1..]),
            });
        }
    }
    out
}

/// A concrete path for a route pattern: `{name}` → `x`, `{*rest}` → `x/y`.
fn concrete(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|seg| {
            if seg.starts_with("{*") {
                "x/y"
            } else if seg.starts_with('{') {
                "x"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[test]
fn the_route_table_is_read() {
    let routes = rynasd_routes();
    assert!(routes.len() > 200, "only {} routes found", routes.len());
    let sizes = routes
        .iter()
        .find(|r| r.path == "/v1/services/ssh")
        .expect("the SSH route");
    assert_eq!(sizes.methods, vec![Method::GET, Method::PUT]);
    assert!(routes.iter().all(|r| !r.methods.is_empty()), "{routes:?}");
    assert_eq!(concrete("/v1/modules/{id}/{*rest}"), "/v1/modules/x/x/y");
}

#[tokio::test]
async fn every_rynasd_route_is_reachable_through_rynas_web() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rynasd.sock");
    let seen: Arc<Mutex<Vec<(Method, String)>>> = Arc::default();
    let rec = seen.clone();
    let fake = axum::Router::new().fallback(move |req: Request| {
        let rec = rec.clone();
        async move {
            rec.lock()
                .unwrap()
                .push((req.method().clone(), req.uri().path().to_string()));
            axum::Json(serde_json::json!({}))
        }
    });
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);
    let creds = app.login().await;

    let mut missing = Vec::new();
    for r in rynasd_routes() {
        if INTERNAL.iter().any(|(p, _)| *p == r.path) {
            continue;
        }
        let path = concrete(&r.path);
        for m in &r.methods {
            seen.lock().unwrap().clear();
            let body = (*m != Method::GET && *m != Method::DELETE).then(|| serde_json::json!({}));
            let uri = format!("/api{path}");
            let resp = app.send(m.clone(), &uri, &creds, body).await;
            let reached = seen
                .lock()
                .unwrap()
                .iter()
                .any(|(sm, sp)| sm == m && *sp == path);
            if !reached {
                missing.push(format!(
                    "{m} {} ({}): rynas-web answered {}",
                    r.path,
                    r.file.display(),
                    resp.status
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "rynasd routes not reachable through rynas-web (add them to proxy::routes, or to \
         INTERNAL with the reason):\n{}",
        missing.join("\n")
    );
}
