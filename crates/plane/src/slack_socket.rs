//! Slack Socket Mode: for Slack connectors with `mode = socket`, the plane opens an outbound
//! WebSocket to Slack instead of receiving webhooks, so it needs no public URL.
//!
//! - Only the replica holding the singleton lock connects ([`Sockets`] lives in its loop), so
//!   each event arrives once; when the lock moves, the new holder connects.
//! - Each tick reconciles the connections with the connectors: new or changed connectors
//!   (config or secrets) get a fresh connection, removed ones or ones switched back to HTTP
//!   are closed.
//! - Per connection: `apps.connections.open` gives a single-use `wss://` URL. Every envelope is
//!   queued as the webhook would queue it, then acknowledged with its `envelope_id`. A
//!   `disconnect` frame or a closed socket reconnects at once; errors back off up to a minute.
//! - De-duplication is unchanged: Slack redelivers unacknowledged envelopes, and chat events are
//!   de-duplicated by their event id in the job queue and in `handle_chat_event`.

use crate::AppState;
use crate::connectors::slack::{self, Slack, SocketFrame};
use crate::connectors::{ChatEvent, Loaded};
use crate::jobs::Job;
use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// Slack pings about every 30 s; without any frame for this long the connection is presumed dead.
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const PING_EVERY: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Queue chat events from Slack (webhook or socket) for the job workers.
pub async fn enqueue(state: &AppState, connector_id: Uuid, evs: Vec<ChatEvent>) -> Result<()> {
    for ev in evs {
        let key = format!("cev:{connector_id}:{}", ev.id);
        state.jobs.push(Job::ChatEvent { connector_id, event: ev }, Some(key)).await?;
    }
    Ok(())
}

/// The Socket Mode connections this replica holds.
#[derive(Default)]
pub struct Sockets {
    tasks: HashMap<Uuid, (String, JoinHandle<()>)>,
}

impl Sockets {
    /// Bring connections in line with the Socket Mode connectors that exist now.
    pub async fn reconcile(&mut self, state: &AppState) -> Result<()> {
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM connectors WHERE kind='slack' AND config->>'mode'='socket'")
            .fetch_all(&state.pool)
            .await?;
        let mut want: HashMap<Uuid, (String, Loaded)> = HashMap::new();
        for id in ids {
            match crate::connectors::load(state, id).await {
                Ok(l) => {
                    want.insert(id, (fingerprint(&l), l));
                }
                Err(e) => tracing::warn!(connector = %id, "slack socket: cannot load connector: {e:#}"),
            }
        }
        self.tasks.retain(|id, (fp, handle)| {
            let keep = !handle.is_finished() && want.get(id).is_some_and(|(w, _)| w == fp);
            if !keep {
                handle.abort();
                tracing::info!(connector = %id, "slack socket: closed");
            }
            keep
        });
        for (id, (fp, l)) in want {
            if self.tasks.contains_key(&id) {
                continue;
            }
            match Slack::from_loaded(&l) {
                Ok(s) => {
                    let handle = tokio::spawn(run_connection(state.clone(), id, l.row.name.clone(), s));
                    self.tasks.insert(id, (fp, handle));
                }
                Err(e) => tracing::warn!(connector = %id, "slack socket: {e:#}"),
            }
        }
        Ok(())
    }

    pub fn stop_all(&mut self) {
        for (_, (_, handle)) in self.tasks.drain() {
            handle.abort();
        }
    }

    pub fn connected(&self) -> usize {
        self.tasks.len()
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Changes to config or secrets restart the connection.
fn fingerprint(l: &Loaded) -> String {
    let mut h = Sha256::new();
    h.update(l.row.config.to_string());
    let mut secrets: Vec<_> = l.secrets.iter().collect();
    secrets.sort();
    for (k, v) in secrets {
        h.update(k);
        h.update([0]);
        h.update(v);
        h.update([0]);
    }
    hex::encode(h.finalize())
}

async fn run_connection(state: AppState, id: Uuid, name: String, slack: Slack) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let deliver = |evs: Vec<ChatEvent>| {
            let state = state.clone();
            async move { enqueue(&state, id, evs).await }
        };
        match session(&slack, &name, deliver).await {
            Ok(reason) => {
                tracing::info!(connector = %id, "slack socket {name}: reconnecting ({reason})");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(connector = %id, "slack socket {name}: {e:#}; retrying in {}s", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// One connection, until Slack asks to reconnect (`Ok`) or something fails (`Err`). Each
/// envelope's events go to `deliver` before the envelope is acknowledged.
async fn session<F, Fut>(slack: &Slack, name: &str, mut deliver: F) -> Result<String>
where
    F: FnMut(Vec<ChatEvent>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let url = slack.open_socket_url().await?;
    let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(tls_connector()?))
        .await
        .context("websocket connect")?;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        let msg = tokio::select! {
            m = tokio::time::timeout(READ_TIMEOUT, ws.next()) => m.map_err(|_| anyhow!("no frames for {}s", READ_TIMEOUT.as_secs()))?,
            _ = ping.tick() => {
                ws.send(Message::Ping(Vec::new().into())).await?;
                continue;
            }
        };
        let Some(msg) = msg else { return Ok("socket ended".into()) };
        match msg? {
            Message::Text(t) => match slack::parse_socket_frame(t.as_str()) {
                Ok(SocketFrame::Hello) => tracing::info!("slack socket {name}: connected"),
                Ok(SocketFrame::Disconnect(reason)) => {
                    let _ = ws.close(None).await;
                    return Ok(format!("disconnect: {reason}"));
                }
                Ok(SocketFrame::Envelope { envelope_id, events }) => {
                    // Queue first, then acknowledge: an envelope lost before the ack is redelivered.
                    deliver(events).await?;
                    ws.send(Message::Text(serde_json::json!({ "envelope_id": envelope_id }).to_string().into())).await?;
                }
                Ok(SocketFrame::Other) => {}
                Err(e) => tracing::warn!("slack socket {name}: unreadable frame: {e:#}"),
            },
            Message::Close(f) => return Ok(format!("closed by Slack{}", f.map(|f| format!(": {}", f.reason)).unwrap_or_default())),
            // Pings are answered by tungstenite.
            _ => {}
        }
    }
}

/// rustls with an explicit provider: the build links both ring and aws-lc-rs, so rustls cannot
/// pick a process default on its own.
fn tls_connector() -> Result<tokio_tungstenite::Connector> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectorRow;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    fn loaded(cfg: Value, secrets: &[(&str, &str)]) -> Loaded {
        Loaded {
            row: ConnectorRow { id: Uuid::nil(), kind: "slack".into(), role: "chat".into(), name: "t".into(), config: cfg, service_identity: None },
            secrets: secrets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    fn slack(api: &str) -> Slack {
        let l = loaded(json!({"api_url": api, "mode": "socket"}), &[("bot_token", "xoxb-bot"), ("app_token", "xapp-app")]);
        assert!(slack::is_socket_mode(&l));
        Slack::from_loaded(&l).unwrap()
    }

    /// A fake Slack: `apps.connections.open` (checks the app token) hands out a local ws:// URL,
    /// whose server sends `hello`, one message envelope and one button envelope, waits for both
    /// acks, then asks the client to reconnect.
    async fn fake_slack() -> (String, Arc<Mutex<Vec<String>>>) {
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}/link", ws.local_addr().unwrap());
        let acks: Arc<Mutex<Vec<String>>> = Arc::default();
        let acks2 = acks.clone();
        tokio::spawn(async move {
            let (tcp, _) = ws.accept().await.unwrap();
            let mut s = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let frame = |v: Value| Message::Text(v.to_string().into());
            s.send(frame(json!({"type": "hello", "num_connections": 1}))).await.unwrap();
            s.send(frame(json!({
                "envelope_id": "env-1", "type": "events_api", "accepts_response_payload": false,
                "payload": {"type": "event_callback", "event_id": "Ev1",
                            "event": {"type": "message", "channel": "C1", "user": "U1", "text": "approve", "ts": "2.0", "thread_ts": "1.0"}}
            })))
            .await
            .unwrap();
            s.send(frame(json!({
                "envelope_id": "env-2", "type": "interactive",
                "payload": {"type": "block_actions", "user": {"id": "U2"}, "channel": {"id": "C1"}, "message": {"ts": "3.0", "thread_ts": "1.0"},
                            "actions": [{"value": "reject:q-9", "action_ts": "4.0"}]}
            })))
            .await
            .unwrap();
            while acks2.lock().unwrap().len() < 2 {
                match s.next().await {
                    Some(Ok(Message::Text(t))) => {
                        let v: Value = serde_json::from_str(t.as_str()).unwrap();
                        acks2.lock().unwrap().push(v["envelope_id"].as_str().unwrap().to_string());
                    }
                    Some(Ok(_)) => {}
                    _ => return, // client went away
                }
            }
            s.send(frame(json!({"type": "disconnect", "reason": "refresh_requested"}))).await.unwrap();
            let _ = s.next().await;
        });
        let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}", api.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/apps.connections.open",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let ws_url = ws_url.clone();
                async move {
                    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
                    if auth != "Bearer xapp-app" {
                        return axum::Json(json!({"ok": false, "error": "invalid_auth"}));
                    }
                    axum::Json(json!({"ok": true, "url": ws_url}))
                }
            }),
        );
        tokio::spawn(async move { axum::serve(api, app).await.unwrap() });
        (api_url, acks)
    }

    #[tokio::test]
    async fn session_delivers_then_acks_and_honours_disconnect() {
        let (api, acks) = fake_slack().await;
        let s = slack(&api);
        let got: Arc<Mutex<Vec<ChatEvent>>> = Arc::default();
        let got2 = got.clone();
        let deliver = move |evs: Vec<ChatEvent>| {
            got2.lock().unwrap().extend(evs);
            async { Ok(()) }
        };
        let reason = tokio::time::timeout(Duration::from_secs(10), session(&s, "test", deliver)).await.expect("session timed out").unwrap();
        assert_eq!(reason, "disconnect: refresh_requested");
        assert_eq!(*acks.lock().unwrap(), vec!["env-1".to_string(), "env-2".to_string()]);
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            (got[0].id.as_str(), got[0].channel.as_str(), got[0].thread_ts.as_deref(), got[0].text.as_str()),
            ("Ev1", "C1", Some("1.0"), "approve")
        );
        assert_eq!(got[1].action, Some(("q-9".to_string(), "rejected".to_string())));
        assert_eq!(got[1].user, "U2");
    }

    #[tokio::test]
    async fn failed_delivery_is_not_acked() {
        let (api, acks) = fake_slack().await;
        let s = slack(&api);
        let r = tokio::time::timeout(Duration::from_secs(10), session(&s, "test", |_| async { Err(anyhow!("queue down")) })).await.unwrap();
        assert!(r.is_err());
        assert!(acks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wrong_app_token_fails_to_open() {
        let (api, _) = fake_slack().await;
        let l = loaded(json!({"api_url": api, "mode": "socket"}), &[("bot_token", "xoxb-bot"), ("app_token", "xapp-wrong")]);
        let e = Slack::from_loaded(&l).unwrap().open_socket_url().await.unwrap_err();
        assert!(format!("{e:#}").contains("invalid_auth"), "{e:#}");
        let l = loaded(json!({"api_url": api, "mode": "socket"}), &[("bot_token", "xoxb-bot")]);
        assert!(Slack::from_loaded(&l).unwrap().open_socket_url().await.is_err());
    }

    /// Network: TLS to Slack's real WebSocket host completes (the bogus ticket is then refused).
    #[tokio::test]
    #[ignore]
    async fn contract_tls_handshake() {
        let r = tokio_tungstenite::connect_async_tls_with_config("wss://wss-primary.slack.com/link/?ticket=invalid", None, false, Some(tls_connector().unwrap())).await;
        match r {
            Ok(_) => {}
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => assert!(resp.status().is_client_error(), "{}", resp.status()),
            Err(e) => panic!("TLS or connect failed: {e}"),
        }
    }

    #[test]
    fn frames() {
        assert!(matches!(slack::parse_socket_frame(r#"{"type":"hello"}"#).unwrap(), SocketFrame::Hello));
        assert!(matches!(slack::parse_socket_frame(r#"{"type":"disconnect","reason":"warning"}"#).unwrap(), SocketFrame::Disconnect(r) if r == "warning"));
        // Bot messages and other envelope types are still acknowledged, with no events.
        let bot = r#"{"envelope_id":"e","type":"events_api","payload":{"type":"event_callback","event":{"type":"message","bot_id":"B1","channel":"C","ts":"1"}}}"#;
        assert!(matches!(slack::parse_socket_frame(bot).unwrap(), SocketFrame::Envelope { envelope_id, events } if envelope_id == "e" && events.is_empty()));
        let cmd = r#"{"envelope_id":"s","type":"slash_commands","payload":{}}"#;
        assert!(matches!(slack::parse_socket_frame(cmd).unwrap(), SocketFrame::Envelope { events, .. } if events.is_empty()));
    }

    #[test]
    fn fingerprint_tracks_secrets_and_config() {
        let a = fingerprint(&loaded(json!({"mode": "socket"}), &[("app_token", "x1")]));
        assert_eq!(a, fingerprint(&loaded(json!({"mode": "socket"}), &[("app_token", "x1")])));
        assert_ne!(a, fingerprint(&loaded(json!({"mode": "socket"}), &[("app_token", "x2")])));
        assert_ne!(a, fingerprint(&loaded(json!({"mode": "socket", "api_url": "http://x"}), &[("app_token", "x1")])));
    }

    #[test]
    fn slack_fields_follow_mode() {
        let info = crate::connectors::kind_info("slack").unwrap();
        let shown = |cfg: Value| -> Vec<&str> {
            let l = loaded(cfg, &[]);
            info.secret_fields.iter().filter(|f| info.applies(&l, f)).map(|f| f.name).collect()
        };
        assert_eq!(shown(json!({})), vec!["bot_token", "signing_secret"]);
        assert_eq!(shown(json!({"mode": "http"})), vec!["bot_token", "signing_secret"]);
        assert_eq!(shown(json!({"mode": "socket"})), vec!["bot_token", "app_token"]);
    }
}
