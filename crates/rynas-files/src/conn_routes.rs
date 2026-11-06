//! The routes that manage the remote connections, answered by rynasd itself (it owns the
//! store): `conns`, `conns/save`, `conns/delete`, `conns/order`, `conns/key`, and its half of
//! `conns/test`. Browsing a connection is the worker's (`rynas-files-worker`).

use std::io::Write;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::conns::{self as remote, Conn, Kind};
use crate::wire::{self as cgi, ApiError, Request};

fn reply(out: &mut impl Write, value: &impl serde::Serialize) -> Result<(), ApiError> {
    cgi::json(out, 200, value)?;
    Ok(())
}

fn post(req: &Request) -> Result<(), ApiError> {
    if req.method == "POST" { Ok(()) } else { Err(ApiError::new(405, "use POST")) }
}

#[derive(Deserialize)]
struct Edit {
    #[serde(default)]
    id: String,
    name: String,
    kind: Kind,
    host: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    path: String,
    /// empty keeps the one saved
    #[serde(default)]
    password: String,
    /// "own": a private key pasted or loaded from a file; empty keeps the one
    /// saved
    #[serde(default)]
    private_key: String,
    /// the private key's passphrase, if it has one: used once, to open it
    #[serde(default)]
    passphrase: String,
    /// forget the server's key remembered (it changed, rightly)
    #[serde(default)]
    forget_key: bool,
}

impl Edit {
    /// The connection this edit describes, with the secrets of the saved one.
    fn apply(self, user: &str, saved: Option<&Conn>) -> Result<Conn, ApiError> {
        let name = self.name.trim();
        let host = self.host.trim().trim_start_matches('\\').trim_end_matches(['\\', '/']);
        if name.is_empty() || name.chars().count() > 80 || name.contains(['\0', '\n']) {
            return Err(ApiError::bad_request(t!("give the connection a name")));
        }
        if host.is_empty() || host.contains(['/', '\\', ' ', '\0']) {
            return Err(ApiError::bad_request(t!("invalid server address")));
        }
        let mut c = saved.cloned().unwrap_or_default();
        let same_server = saved.is_some_and(|s| s.host.eq_ignore_ascii_case(host) && s.port == self.port && s.kind == self.kind);
        c.id = if self.id.is_empty() { remote::new_id() } else { self.id };
        c.name = name.to_string();
        c.kind = self.kind;
        c.host = host.to_string();
        c.port = self.port;
        c.domain = self.domain.trim().to_string();
        c.user = self.user.trim().to_string();
        c.auth = match (self.kind, self.auth.as_str()) {
            (Kind::Sftp, "key" | "own") => self.auth.clone(),
            _ => "password".into(),
        };
        if c.auth != "own" {
            c.ssh_key.clear();
            c.key_info.clear();
        } else if !self.private_key.trim().is_empty() {
            let pass = (!self.passphrase.is_empty()).then_some(self.passphrase.as_str());
            let (private, info) = remote::import_key(user, self.private_key.trim(), pass).map_err(|e| ApiError::invalid("private_key", e))?;
            c.ssh_key = private;
            c.key_info = info;
        } else if c.ssh_key.is_empty() {
            return Err(ApiError::bad_request(t!("load the private key, or paste it")));
        }
        c.path = self.path.trim().replace('\\', "/").trim_matches('/').to_string();
        if !self.password.is_empty() {
            c.password = self.password;
        }
        if self.forget_key || !same_server {
            c.host_key.clear();
        }
        Ok(c)
    }
}

/// The user's connections as the interface sees them (no secrets), and
/// the public half of the user's SSH key.
pub fn list(user: &str) -> Value {
    public_list(user, &remote::load(user))
}

fn public_list(user: &str, list: &[Conn]) -> Value {
    let key = remote::ssh_key(user, false).ok().flatten().map(|(_, public)| public);
    json!({ "conns": list.iter().map(Conn::public).collect::<Vec<_>>(), "key": key })
}

/// The routes `conns`, `conns/save`, `conns/delete`, `conns/order`, `conns/key`: run by
/// rynasd in its own process (it owns the store).
pub fn conns(req: &Request, out: &mut impl Write, user: &str) -> Result<(), ApiError> {
    let mut list = remote::load(user);
    match req.route.as_str() {
        "conns" => reply(out, &public_list(user, &list)),
        "conns/save" => {
            post(req)?;
            let edit: Edit = req.json()?;
            let saved = list.iter().position(|c| c.id == edit.id && !edit.id.is_empty());
            let mut c = edit.apply(user, saved.map(|i| &list[i]))?;
            // an SFTP server's key is remembered now (learnt without logging in), so a
            // later change shows
            if c.kind == Kind::Sftp && c.host_key.is_empty() {
                let _ = remote::pin_host_key(user, &mut c);
            }
            match saved {
                Some(i) => list[i] = c.clone(),
                None => list.push(c.clone()),
            }
            remote::store(user, &list)?;
            reply(out, &json!({ "conn": c.public() }))
        }
        "conns/delete" => {
            post(req)?;
            let body: Value = req.json()?;
            let id = body["id"].as_str().unwrap_or_default();
            list.retain(|c| c.id != id);
            remote::store(user, &list)?;
            reply(out, &json!({ "ok": true }))
        }
        "conns/order" => {
            post(req)?;
            let ids: Vec<String> = req.json()?;
            list.sort_by_key(|c| ids.iter().position(|i| *i == c.id).unwrap_or(usize::MAX));
            remote::store(user, &list)?;
            reply(out, &json!({ "ok": true }))
        }
        "conns/key" => {
            post(req)?;
            let (_, public) = remote::ssh_key(user, true)?.ok_or_else(|| ApiError::new(500, "no key"))?;
            reply(out, &json!({ "key": public }))
        }
        _ => Err(ApiError::not_found(format!("no route {}", req.route))),
    }
}

/// `conns/test`, rynasd's half: the connection the form describes, with the secrets of
/// the saved one and the user's key; a saved SFTP server whose key is not known yet
/// (out of reach when saved) gets it pinned now. The worker then logs in ([`test`]).
pub fn test_conn(req: &Request, user: &str) -> Result<Conn, ApiError> {
    post(req)?;
    let list = remote::load(user);
    let edit: Edit = req.json()?;
    let saved = list.iter().find(|c| c.id == edit.id && !edit.id.is_empty()).cloned();
    let mut c = with_key(user, edit.apply(user, saved.as_ref())?);
    if list.iter().any(|s| s.id == c.id && !c.id.is_empty() && s.host_key.is_empty()) {
        let _ = remote::pin_host_key(user, &mut c);
    }
    Ok(c)
}

/// The connection with the user's SSH key, when it logs in with it.
pub fn with_key(user: &str, c: Conn) -> Conn {
    remote::with_user_key(user, c)
}

/// The remote path a request is about, if it is about one.
pub fn remote_param(req: &Request) -> Option<&str> {
    let p = req.param("path").or(req.param("dir"))?;
    remote::split(p).is_some().then_some(p)
}

pub fn first_of_paths(req: &Request) -> Option<Vec<String>> {
    let list: Vec<String> = serde_json::from_str(req.param("paths")?).ok()?;
    list.first().is_some_and(|p| remote::split(p).is_some()).then_some(list)
}

/// Whether a request may be about a remote path, so the connections have
/// to be read before the CGI becomes the user. `usage` has its paths in
/// the body.
pub fn wants_conns(req: &Request) -> bool {
    req.route == "usage" || remote_param(req).is_some() || (req.route == "download" && first_of_paths(req).is_some())
}

