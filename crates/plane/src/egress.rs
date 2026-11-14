//! The factory egress proxy. Runners have no direct internet access; this
//! proxy is their only way out.
//!
//! - Forward proxy: `CONNECT host:port` (HTTPS) and absolute-form plain HTTP
//!   requests are allowed only to domains on the administrator-managed
//!   allowlist (`settings.egress_allowlist`; an entry allows the domain and
//!   its subdomains). Everything else gets 403.
//! - Private package feeds: requests to `/feeds/<name>/<path>` are proxied to
//!   the feed's upstream with the feed credential added here. Feeds are
//!   `package_feed` connectors (config `name`, `upstream`; secret
//!   `auth_header`). The runner never sees the credential.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

#[derive(Clone, Default)]
pub struct Feed {
    pub upstream: String,
    pub auth_header: Option<String>,
}

#[derive(Clone, Default)]
pub struct Policy {
    pub allow: Vec<String>,
    pub feeds: HashMap<String, Feed>,
}

pub fn allowed(allow: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allow.iter().any(|a| {
        let a = a.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase();
        !a.is_empty() && (host == a || host.ends_with(&format!(".{a}")))
    })
}

fn split_host_port(s: &str, default: u16) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() && !h.ends_with(']') || s.starts_with('[') && s.contains("]:") => {
            (h.trim_matches(['[', ']']).to_string(), p.parse().unwrap_or(default))
        }
        _ => (s.trim_matches(['[', ']']).to_string(), default),
    }
}

async fn respond(s: &mut TcpStream, code: u16, reason: &str, body: &str) {
    let r = format!("HTTP/1.1 {code} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
    let _ = s.write_all(r.as_bytes()).await;
}

pub async fn handle(mut client: TcpStream, policy: Arc<RwLock<Policy>>, http: reqwest::Client) -> Result<()> {
    let mut reader = BufReader::new(&mut client);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        head.push_str(&line);
        if line == "\r\n" || line == "\n" {
            break;
        }
        if head.len() > 64 * 1024 {
            return Err(anyhow!("request head too large"));
        }
    }
    let buffered = reader.buffer().to_vec();
    drop(reader);
    let first = head.lines().next().unwrap_or("").to_string();
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let pol = policy.read().await.clone();

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(&target, 443);
        if !allowed(&pol.allow, &host) {
            tracing::info!(%host, "egress denied");
            respond(&mut client, 403, "Forbidden", &format!("egress to {host} is not on the allowlist\n")).await;
            return Ok(());
        }
        let mut up = TcpStream::connect((host.as_str(), port)).await?;
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        if !buffered.is_empty() {
            up.write_all(&buffered).await?;
        }
        tracing::info!(%host, port, "egress tunnel");
        let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
        return Ok(());
    }

    // Private feeds (origin-form request to the proxy itself).
    if let Some(rest) = target.strip_prefix("/feeds/") {
        let (name, path) = rest.split_once('/').unwrap_or((rest, ""));
        let Some(feed) = pol.feeds.get(name) else {
            respond(&mut client, 404, "Not Found", "unknown feed\n").await;
            return Ok(());
        };
        let url = format!("{}/{path}", feed.upstream.trim_end_matches('/'));
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut rb = http.request(m, &url);
        for l in head.lines().skip(1) {
            if let Some((k, v)) = l.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                if !["host", "authorization", "proxy-authorization", "connection", "content-length"].contains(&k.as_str()) {
                    rb = rb.header(k, v.trim());
                }
            }
        }
        if let Some(h) = &feed.auth_header {
            rb = rb.header("authorization", h);
        }
        let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
        if len > 0 {
            let mut body = buffered.clone();
            while body.len() < len {
                let mut b = vec![0u8; len - body.len()];
                let n = client.read(&mut b).await?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&b[..n]);
            }
            rb = rb.body(body);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
        let bytes = resp.bytes().await?;
        let h = format!("HTTP/1.1 {} {}\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", status.as_u16(), status.canonical_reason().unwrap_or(""), bytes.len());
        client.write_all(h.as_bytes()).await?;
        client.write_all(&bytes).await?;
        return Ok(());
    }

    // Absolute-form plain HTTP.
    let Ok(u) = url::Url::parse(&target) else {
        respond(&mut client, 400, "Bad Request", "expected a proxy request\n").await;
        return Ok(());
    };
    let host = u.host_str().unwrap_or("").to_string();
    if !allowed(&pol.allow, &host) {
        tracing::info!(%host, "egress denied");
        respond(&mut client, 403, "Forbidden", &format!("egress to {host} is not on the allowlist\n")).await;
        return Ok(());
    }
    let port = u.port_or_known_default().unwrap_or(80);
    let mut up = TcpStream::connect((host.as_str(), port)).await?;
    let origin = format!("{}{}", u.path(), u.query().map(|q| format!("?{q}")).unwrap_or_default());
    let rest: String = head.splitn(2, '\n').nth(1).unwrap_or("").to_string();
    let rewritten = format!("{method} {origin} HTTP/1.1\r\n{}", rest.lines().filter(|l| !l.to_ascii_lowercase().starts_with("proxy-")).map(|l| format!("{l}\r\n")).collect::<String>().trim_end_matches("\r\n\r\n").to_string() + "\r\n\r\n");
    up.write_all(rewritten.as_bytes()).await?;
    if !buffered.is_empty() {
        up.write_all(&buffered).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
    Ok(())
}

/// Load the allowlist and feeds from the database.
pub async fn load_policy(pool: &sqlx::PgPool, crypto: &crate::crypto::Crypto) -> Result<Policy> {
    let s = crate::settings::get(pool).await?;
    let mut feeds = HashMap::new();
    let rows: Vec<(uuid::Uuid, serde_json::Value)> = sqlx::query_as("SELECT id, config FROM connectors WHERE kind='package_feed'").fetch_all(pool).await?;
    for (id, cfg) in rows {
        let name = cfg["name"].as_str().unwrap_or("").to_string();
        let upstream = cfg["upstream"].as_str().unwrap_or("").to_string();
        let sec: Option<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as("SELECT key_id, nonce, ciphertext FROM secrets WHERE connector_id=$1 AND name='auth_header'").bind(id).fetch_optional(pool).await?;
        let auth_header = match sec {
            Some((k, n, c)) => Some(String::from_utf8(crypto.decrypt(&k, &n, &c)?)?),
            None => None,
        };
        if !name.is_empty() {
            feeds.insert(name, Feed { upstream, auth_header });
        }
    }
    Ok(Policy { allow: s.egress_allowlist, feeds })
}

pub async fn serve(listener: TcpListener, policy: Arc<RwLock<Policy>>) -> Result<()> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?;
    loop {
        let (sock, _) = listener.accept().await?;
        let p = policy.clone();
        let h = http.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, p, h).await {
                tracing::debug!("egress connection error: {e:#}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matching() {
        let a = vec!["pypi.org".to_string(), ".npmjs.org".to_string(), "*.crates.io".to_string()];
        assert!(allowed(&a, "pypi.org"));
        assert!(allowed(&a, "files.pypi.org"));
        assert!(allowed(&a, "registry.npmjs.org"));
        assert!(allowed(&a, "static.crates.io"));
        assert!(!allowed(&a, "evilpypi.org"));
        assert!(!allowed(&a, "example.com"));
        assert!(!allowed(&a, "pypi.org.evil.com"));
    }

    async fn proxy(policy: Policy) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, Arc::new(RwLock::new(policy))));
        port
    }

    #[tokio::test]
    async fn denies_off_list_and_tunnels_on_list() {
        // Upstream that echoes a fixed response.
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = up.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = up.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut b = [0u8; 4096];
                    let _ = s.read(&mut b).await;
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
                });
            }
        });
        let port = proxy(Policy { allow: vec!["localhost".into()], feeds: HashMap::new() }).await;
        let via = |p: &str| reqwest::Client::builder().proxy(reqwest::Proxy::http(format!("http://127.0.0.1:{p}")).unwrap()).build().unwrap();
        let c = via(&port.to_string());
        let r = c.get(format!("http://localhost:{up_port}/x")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "ok");
        let r = c.get(format!("http://127.0.0.1:{up_port}/x")).send().await.unwrap();
        assert_eq!(r.status(), 403, "IP literal not on allowlist");
        // CONNECT to a denied host.
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n").await.unwrap();
        let mut b = vec![0u8; 256];
        let n = s.read(&mut b).await.unwrap();
        assert!(String::from_utf8_lossy(&b[..n]).starts_with("HTTP/1.1 403"));
    }

    #[tokio::test]
    async fn feeds_get_the_credential_added() {
        use axum::{Router, http::HeaderMap, routing::get};
        let app = Router::new().route("/simple/pkg/", get(|h: HeaderMap| async move { h.get("authorization").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default() }));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let mut feeds = HashMap::new();
        feeds.insert("private".to_string(), Feed { upstream: format!("http://127.0.0.1:{up}"), auth_header: Some("Bearer feed-secret".into()) });
        let port = proxy(Policy { allow: vec![], feeds }).await;
        let r = reqwest::get(format!("http://127.0.0.1:{port}/feeds/private/simple/pkg/")).await.unwrap();
        assert_eq!(r.text().await.unwrap(), "Bearer feed-secret");
    }
}
