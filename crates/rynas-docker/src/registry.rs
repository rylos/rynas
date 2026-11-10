//! The digest of a tag straight from its registry: `HEAD /v2/<repo>/manifests/<tag>` with
//! the four manifest media types in `Accept`, reading `Docker-Content-Digest`. A HEAD does not
//! count against Docker Hub's anonymous pull limit (a GET of the manifest does), and the
//! digest is the one of the index (multi-platform list) when the tag has one, so it compares
//! with the local image's repo digest, never with a per-platform digest.
//!
//! Anonymous only: a `401` gets a token from the realm the registry names
//! (`WWW-Authenticate: Bearer realm=…,service=…,scope=…`), as `docker pull` does for public
//! images. A `429` gives [`RegistryError::RateLimited`] with the `Retry-After` delay.

use std::time::Duration;

use crate::updates::split_ref;

/// The manifest types a registry may answer for a tag, index first.
pub const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.oci.image.manifest.v1+json";

const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// `429`: wait this long before the next request to the registry.
    RateLimited(Duration),
    /// No digest this way (private image, registry without the header…): another way may
    /// work (the engine's distribution endpoint).
    Unsupported(String),
    /// Network or registry failure.
    Failed(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::RateLimited(d) => {
                write!(f, "the registry limits requests: retry in {} s", d.as_secs())
            }
            RegistryError::Unsupported(m) | RegistryError::Failed(m) => f.write_str(m),
        }
    }
}

/// Where a normalized repository (`docker.io/library/nginx`) lives: `(host, path)`.
pub fn endpoint(repo: &str) -> (String, String) {
    let (host, path) = repo.split_once('/').unwrap_or(("docker.io", repo));
    let host = match host {
        "docker.io" | "index.docker.io" => "registry-1.docker.io",
        h => h,
    };
    (host.to_string(), path.to_string())
}

/// The registry host of an image reference (one rate limit per host).
pub fn host_of(image: &str) -> String {
    endpoint(&split_ref(image).0).0
}

/// The parameters of a `WWW-Authenticate: Bearer k="v",…` header.
pub fn bearer_params(h: &str) -> Option<Vec<(String, String)>> {
    let rest = h.trim().strip_prefix("Bearer ").or_else(|| h.trim().strip_prefix("bearer "))?;
    let mut out = Vec::new();
    let mut s = rest.trim();
    while !s.is_empty() {
        let (k, after) = s.split_once('=')?;
        let after = after.trim_start();
        let (v, next) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (&q[..end], q[end + 1..].trim_start())
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim(), &after[end..])
        };
        out.push((k.trim().to_string(), v.to_string()));
        s = next.trim_start_matches(',').trim_start();
    }
    Some(out)
}

/// `Retry-After` in seconds (an HTTP date is taken as one minute), at most 15 minutes.
pub fn retry_after(v: Option<&str>) -> Duration {
    let s = v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(60);
    Duration::from_secs(s.clamp(1, 900))
}

/// Percent-encoding of a query value (everything but the unreserved characters).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The most a token answer may be (a bearer token is a few KiB).
const TOKEN_MAX: usize = 64 * 1024;

/// The client of the token requests: the realm comes from the registry's header, so a
/// redirect elsewhere is not followed (a 3xx is "no anonymous token").
fn token_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

/// The body of `r`, refused past `cap` bytes (by `Content-Length` before reading, and while
/// reading).
async fn read_capped(mut r: reqwest::Response, cap: usize) -> Result<Vec<u8>, RegistryError> {
    let too_big = || RegistryError::Failed(format!("token answer: more than {} KiB", cap / 1024));
    if r.content_length().is_some_and(|n| n > cap as u64) {
        return Err(too_big());
    }
    let mut body = Vec::new();
    while let Some(chunk) = r
        .chunk()
        .await
        .map_err(|e| RegistryError::Failed(format!("token answer: {e}")))?
    {
        if body.len() + chunk.len() > cap {
            return Err(too_big());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A small HTTPS client for manifest HEAD requests (a HEAD answer has no body to read).
#[derive(Clone)]
pub struct Registry {
    http: reqwest::Client,
    token_http: reqwest::Client,
}

impl Registry {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            token_http: token_client()?,
        })
    }

    async fn head(&self, url: &str, token: Option<&str>) -> Result<reqwest::Response, RegistryError> {
        let mut rq = self.http.head(url).header("Accept", ACCEPT);
        if let Some(t) = token {
            rq = rq.bearer_auth(t);
        }
        rq.send()
            .await
            .map_err(|e| RegistryError::Failed(format!("registry not reachable: {e}")))
    }

    async fn token(&self, header: &str) -> Result<String, RegistryError> {
        let params = bearer_params(header)
            .ok_or_else(|| RegistryError::Unsupported("the registry asks for a login".into()))?;
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let realm = get("realm")
            .filter(|r| r.starts_with("https://"))
            .ok_or_else(|| RegistryError::Unsupported("no usable token realm".into()))?;
        let mut url = realm;
        let mut sep = if url.contains('?') { '&' } else { '?' };
        for k in ["service", "scope"] {
            if let Some(v) = get(k) {
                url.push(sep);
                url.push_str(&format!("{k}={}", enc(&v)));
                sep = '&';
            }
        }
        let r = self
            .token_http
            .get(&url)
            .send()
            .await
            .map_err(|e| RegistryError::Failed(format!("token service not reachable: {e}")))?;
        if r.status().as_u16() == 429 {
            return Err(RegistryError::RateLimited(retry_after(
                r.headers().get("retry-after").and_then(|v| v.to_str().ok()),
            )));
        }
        if !r.status().is_success() {
            return Err(RegistryError::Unsupported(format!(
                "no anonymous token ({})",
                r.status().as_u16()
            )));
        }
        let body = read_capped(r, TOKEN_MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| RegistryError::Failed(format!("token answer: {e}")))?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .ok_or_else(|| RegistryError::Unsupported("the token service gave no token".into()))
    }

    /// The digest the registry has for `image` (a tag reference) now.
    pub async fn digest(&self, image: &str) -> Result<String, RegistryError> {
        let (repo, tag) = split_ref(image);
        let tag = tag.ok_or_else(|| RegistryError::Unsupported("not a tag".into()))?;
        let (host, path) = endpoint(&repo);
        let url = format!("https://{host}/v2/{path}/manifests/{tag}");
        let mut r = self.head(&url, None).await?;
        if r.status().as_u16() == 401 {
            let h = r
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let token = self.token(&h).await?;
            r = self.head(&url, Some(&token)).await?;
        }
        let status = r.status().as_u16();
        match status {
            200 => r
                .headers()
                .get("docker-content-digest")
                .and_then(|v| v.to_str().ok())
                .filter(|d| d.starts_with("sha256:"))
                .map(str::to_string)
                .ok_or_else(|| RegistryError::Unsupported("the registry sent no digest".into())),
            429 => Err(RegistryError::RateLimited(retry_after(
                r.headers().get("retry-after").and_then(|v| v.to_str().ok()),
            ))),
            401 | 403 => Err(RegistryError::Unsupported(
                "the registry refuses anonymous access (private image)".into(),
            )),
            404 => Err(RegistryError::Failed(format!("{image}: the tag is not in the registry"))),
            s => Err(RegistryError::Failed(format!("the registry answered {s}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(
            endpoint("docker.io/library/nginx"),
            ("registry-1.docker.io".into(), "library/nginx".into())
        );
        assert_eq!(
            endpoint("ghcr.io/home-assistant/home-assistant"),
            ("ghcr.io".into(), "home-assistant/home-assistant".into())
        );
        assert_eq!(host_of("nginx:1.27"), "registry-1.docker.io");
        assert_eq!(host_of("registry.local:5000/a/b:1"), "registry.local:5000");
    }

    #[test]
    fn auth_header() {
        let p = bearer_params(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
        )
        .unwrap();
        assert_eq!(p[0], ("realm".into(), "https://auth.docker.io/token".into()));
        assert_eq!(p[2].1, "repository:library/nginx:pull");
        assert!(bearer_params("Basic realm=\"x\"").is_none());
        assert_eq!(enc("repository:library/nginx:pull"), "repository%3Alibrary%2Fnginx%3Apull");
        assert_eq!(retry_after(Some("120")), Duration::from_secs(120));
        assert_eq!(retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")), Duration::from_secs(60));
        assert_eq!(retry_after(Some("99999")), Duration::from_secs(900));
    }

    /// A one-shot HTTP server answering `answer` to the first request.
    async fn serve_once(answer: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(&answer).await;
            let _ = s.shutdown().await;
        });
        format!("http://{addr}/token")
    }

    #[tokio::test]
    async fn token_answers_are_capped_and_not_redirected() {
        let c = token_client().unwrap();
        // a redirect is handed back, not followed
        let url = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/x\r\nContent-Length: 0\r\n\r\n".to_vec(),
        )
        .await;
        assert_eq!(c.get(&url).send().await.unwrap().status().as_u16(), 302);
        // a small answer is read
        let url = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"token\":\"t\"}".to_vec()).await;
        let r = c.get(&url).send().await.unwrap();
        assert_eq!(read_capped(r, TOKEN_MAX).await.unwrap(), b"{\"token\":\"t\"}");
        // too long by Content-Length
        let url = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 70000\r\n\r\n".to_vec()).await;
        let r = c.get(&url).send().await.unwrap();
        assert!(read_capped(r, TOKEN_MAX).await.is_err());
        // too long without Content-Length (read until the connection closes)
        let mut big = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        big.extend(std::iter::repeat_n(b'a', TOKEN_MAX + 1));
        let url = serve_once(big).await;
        let r = c.get(&url).send().await.unwrap();
        assert!(read_capped(r, TOKEN_MAX).await.is_err());
    }
}
