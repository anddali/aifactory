//! Slack chat connector (Web API).
//!
//! Configuration: `api_url` (default `https://slack.com/api`). Secrets: `bot_token` (`xoxb-…`, sent as
//! `Authorization: Bearer`) and `signing_secret` (used by [`verify_signature`]).
//! Bot scopes needed: `chat:write`, `channels:read`, `groups:read`, `channels:history`,
//! `groups:history`, `users:read`, `users:read.email`.
//!
//! Conventions:
//! - Channels may be given as IDs (`C0123…`) or names (`#factory` / `factory`); names are resolved
//!   to IDs with `conversations.list` (cached per connector instance). Returned [`ChatRef`]s always
//!   carry the channel ID, which is also what inbound events carry.
//! - Markers travel in message metadata: `{"event_type": "factory_marker", "event_payload": {"marker": …}}`.
//!   [`Slack::find_by_marker`] scans recent channel history and the threads in it with
//!   `include_all_metadata=true`.
//! - Approval buttons are a Block Kit `actions` block with values `approve:<question id>` and
//!   `reject:<question id>`. [`Slack::update`] replaces the text and removes the buttons.
//!
//! Contract tests (`#[ignore]`) read `FACTORY_CONTRACT_SLACK_BOT_TOKEN` and
//! `FACTORY_CONTRACT_SLACK_CHANNEL` (a name like `#factory-test` or an ID); they post into that channel.

use super::*;
use axum::http::HeaderMap;
use reqwest::{Method, StatusCode};
use std::sync::Mutex;
use std::time::Duration;

pub const MARKER_EVENT_TYPE: &str = "factory_marker";
const HISTORY_PAGES: usize = 5;
const MAX_THREADS: usize = 50;
const REPLAY_WINDOW_SECS: i64 = 300;

pub struct Slack {
    http: reqwest::Client,
    api: String,
    token: String,
    channels: Mutex<HashMap<String, String>>,
}

/// What a Slack request to the plane's webhook endpoint carries.
#[derive(Debug)]
pub enum SlackInbound {
    /// Events API URL verification: respond with this challenge.
    UrlVerification(String),
    Events(Vec<ChatEvent>),
}

impl Slack {
    pub fn from_loaded(l: &Loaded) -> Result<Self> {
        let api = l.cfg("api_url").unwrap_or_else(|| "https://slack.com/api".into());
        Ok(Slack {
            http: reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?,
            api: api.trim_end_matches('/').to_string(),
            token: l.secret("bot_token")?,
            channels: Mutex::new(HashMap::new()),
        })
    }

    /// Call a Web API method and return the body without checking `ok`. Read methods use GET with
    /// query parameters, write methods a JSON body. Retries on 429 honouring `Retry-After`.
    async fn call_raw(&self, method: &str, http: Method, params: &Value) -> Result<Value> {
        let url = format!("{}/{method}", self.api);
        let mut attempt = 0;
        loop {
            let mut rb = self.http.request(http.clone(), &url).bearer_auth(&self.token);
            if http == Method::GET {
                let q: Vec<(String, String)> = params
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
                    .collect();
                rb = rb.query(&q);
            } else {
                rb = rb.json(params);
            }
            let resp = rb.send().await?;
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < 3 {
                attempt += 1;
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let text = resp.text().await?;
            if !status.is_success() {
                let snippet: String = text.chars().take(300).collect();
                bail!("slack {method} returned {status}: {snippet}");
            }
            return Ok(serde_json::from_str(&text)?);
        }
    }

    async fn call(&self, method: &str, http: Method, params: &Value) -> Result<Value> {
        let v = self.call_raw(method, http, params).await?;
        if v["ok"].as_bool() != Some(true) {
            bail!("slack {method} failed: {}", v["error"].as_str().unwrap_or("unknown error"));
        }
        Ok(v)
    }

    /// Resolve a channel name to its ID (IDs pass through).
    async fn channel_id(&self, channel: &str) -> Result<String> {
        let c = channel.trim();
        if is_channel_id(c) {
            return Ok(c.to_string());
        }
        let name = c.trim_start_matches('#').to_lowercase();
        if let Some(id) = self.channels.lock().unwrap().get(&name) {
            return Ok(id.clone());
        }
        let mut cursor: Option<String> = None;
        let mut found = None;
        for _ in 0..50 {
            let v = self
                .call(
                    "conversations.list",
                    Method::GET,
                    &json!({"types": "public_channel,private_channel", "exclude_archived": true, "limit": 1000, "cursor": cursor}),
                )
                .await?;
            {
                let mut cache = self.channels.lock().unwrap();
                for ch in v["channels"].as_array().into_iter().flatten() {
                    if let (Some(n), Some(id)) = (ch["name"].as_str(), ch["id"].as_str()) {
                        cache.insert(n.to_lowercase(), id.to_string());
                        if n.eq_ignore_ascii_case(&name) {
                            found = Some(id.to_string());
                        }
                    }
                }
            }
            if found.is_some() {
                break;
            }
            cursor = v["response_metadata"]["next_cursor"].as_str().filter(|s| !s.is_empty()).map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        found.ok_or_else(|| anyhow!("slack channel {channel} not found (is the bot a member, with channels:read/groups:read?)"))
    }

    /// Messages of a channel's recent history or of a thread, newest pages first.
    async fn messages(&self, method: &str, params: Value) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..HISTORY_PAGES {
            let mut p = params.clone();
            p["cursor"] = cursor.clone().map(Value::String).unwrap_or(Value::Null);
            let v = self.call(method, Method::GET, &p).await?;
            out.extend(v["messages"].as_array().cloned().unwrap_or_default());
            cursor = v["response_metadata"]["next_cursor"].as_str().filter(|s| !s.is_empty()).map(String::from);
            if cursor.is_none() || v["has_more"].as_bool() == Some(false) {
                break;
            }
        }
        Ok(out)
    }
}

fn has_marker(m: &Value, marker: &str) -> bool {
    m["metadata"]["event_type"].as_str() == Some(MARKER_EVENT_TYPE) && m["metadata"]["event_payload"]["marker"].as_str() == Some(marker)
}

fn to_ref(channel: &str, m: &Value) -> ChatRef {
    let ts = m["ts"].as_str().unwrap_or_default().to_string();
    let thread_ts = m["thread_ts"].as_str().filter(|t| *t != ts).map(String::from);
    ChatRef { channel: channel.into(), ts, thread_ts }
}

fn is_channel_id(s: &str) -> bool {
    s.len() >= 9 && matches!(s.as_bytes()[0], b'C' | b'G' | b'D') && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `<@U…>` mentions for a mention string (one id, or several separated by commas/spaces).
fn mention_prefix(m: &str) -> String {
    m.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| if s.starts_with('<') { s.to_string() } else { format!("<@{}>", s.trim_start_matches('@')) })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// The `chat.postMessage` body for a message.
fn post_body(channel_id: &str, thread: Option<&ChatRef>, msg: &ChatMessage) -> Value {
    let text = match msg.mention.as_deref().map(mention_prefix).filter(|p| !p.is_empty()) {
        Some(p) => format!("{p} {}", msg.text),
        None => msg.text.clone(),
    };
    let mut body = json!({"channel": channel_id, "text": text, "unfurl_links": false});
    if let Some(t) = thread {
        body["thread_ts"] = json!(t.thread_ts.clone().unwrap_or_else(|| t.ts.clone()));
    }
    if let Some(m) = &msg.marker {
        body["metadata"] = json!({"event_type": MARKER_EVENT_TYPE, "event_payload": {"marker": m}});
    }
    if let Some(q) = &msg.approval_for {
        body["blocks"] = json!([
            {"type": "section", "text": {"type": "mrkdwn", "text": truncate(&text, 3000)}},
            {"type": "actions", "block_id": format!("factory_approval:{q}"), "elements": [
                {"type": "button", "action_id": "factory_approve", "style": "primary",
                 "text": {"type": "plain_text", "text": "Approve"}, "value": format!("approve:{q}")},
                {"type": "button", "action_id": "factory_reject", "style": "danger",
                 "text": {"type": "plain_text", "text": "Reject"}, "value": format!("reject:{q}")}
            ]}
        ]);
    }
    body
}

#[async_trait]
impl Chat for Slack {
    async fn test_connection(&self) -> Result<String> {
        let v = self.call("auth.test", Method::POST, &json!({})).await?;
        Ok(format!(
            "connected to Slack workspace {} as {}",
            v["team"].as_str().unwrap_or("?"),
            v["user"].as_str().unwrap_or("?")
        ))
    }

    async fn post(&self, channel: &str, thread: Option<&ChatRef>, msg: &ChatMessage) -> Result<ChatRef> {
        let id = self.channel_id(channel).await?;
        let body = post_body(&id, thread, msg);
        let v = self.call("chat.postMessage", Method::POST, &body).await?;
        let ts = v["ts"].as_str().ok_or_else(|| anyhow!("slack chat.postMessage returned no ts"))?.to_string();
        Ok(ChatRef {
            channel: v["channel"].as_str().unwrap_or(&id).to_string(),
            ts,
            thread_ts: body["thread_ts"].as_str().map(String::from),
        })
    }

    async fn update(&self, msg_ref: &ChatRef, text: &str) -> Result<()> {
        let id = self.channel_id(&msg_ref.channel).await?;
        // An empty blocks array removes approval buttons; metadata is retained by Slack.
        self.call("chat.update", Method::POST, &json!({"channel": id, "ts": msg_ref.ts, "text": text, "blocks": []})).await?;
        Ok(())
    }

    async fn find_by_marker(&self, channel: &str, marker: &str) -> Result<Option<ChatRef>> {
        let id = self.channel_id(channel).await?;
        let history = self.messages("conversations.history", json!({"channel": id, "limit": 200, "include_all_metadata": true})).await?;
        if let Some(m) = history.iter().find(|m| has_marker(m, marker)) {
            return Ok(Some(to_ref(&id, m)));
        }
        let threads: Vec<&str> = history
            .iter()
            .filter(|m| m["reply_count"].as_u64().unwrap_or(0) > 0)
            .filter_map(|m| m["ts"].as_str())
            .take(MAX_THREADS)
            .collect();
        for ts in threads {
            let replies = self
                .messages("conversations.replies", json!({"channel": id, "ts": ts, "limit": 200, "include_all_metadata": true}))
                .await?;
            if let Some(m) = replies.iter().find(|m| has_marker(m, marker)) {
                return Ok(Some(to_ref(&id, m)));
            }
        }
        Ok(None)
    }

    async fn lookup_user_by_email(&self, email: &str) -> Result<Option<String>> {
        let v = self.call_raw("users.lookupByEmail", Method::GET, &json!({"email": email})).await?;
        if v["ok"].as_bool() == Some(true) {
            return Ok(v["user"]["id"].as_str().map(String::from));
        }
        match v["error"].as_str() {
            Some("users_not_found") => Ok(None),
            e => bail!("slack users.lookupByEmail failed: {}", e.unwrap_or("unknown error")),
        }
    }
}

// ---------- webhooks ----------

/// Slack request signing (v0): `X-Slack-Signature: v0=<hex HMAC-SHA256 of "v0:{timestamp}:{body}">`
/// with `X-Slack-Request-Timestamp` within five minutes of now.
pub fn verify_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    verify_signature_at(secret, headers, body, chrono::Utc::now().timestamp())
}

fn verify_signature_at(secret: &str, headers: &HeaderMap, body: &[u8], now: i64) -> bool {
    use hmac::{Hmac, KeyInit, Mac};
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::trim);
    let (Some(ts), Some(sig)) = (h("x-slack-request-timestamp"), h("x-slack-signature")) else { return false };
    let Ok(t) = ts.parse::<i64>() else { return false };
    if (now - t).abs() > REPLAY_WINDOW_SECS {
        return false;
    }
    let Some(hexsig) = sig.strip_prefix("v0=") else { return false };
    let Ok(expected) = hex::decode(hexsig) else { return false };
    let Ok(mut mac) = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(format!("v0:{ts}:").as_bytes());
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Parse an Events API delivery (JSON) or an interactivity request (form-encoded `payload=`).
pub fn parse_webhook(headers: &HeaderMap, body: &[u8]) -> Result<SlackInbound> {
    let form = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"))
        || body.starts_with(b"payload=");
    if form {
        let payload = url::form_urlencoded::parse(body)
            .find(|(k, _)| k == "payload")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| anyhow!("slack interactive request without payload"))?;
        let v: Value = serde_json::from_str(&payload)?;
        return Ok(SlackInbound::Events(parse_interaction(&v)));
    }
    let v: Value = serde_json::from_slice(body)?;
    match v["type"].as_str().unwrap_or_default() {
        "url_verification" => Ok(SlackInbound::UrlVerification(v["challenge"].as_str().unwrap_or_default().to_string())),
        "event_callback" => {
            let e = &v["event"];
            let user_message = e["type"] == "message"
                && e.get("subtype").is_none()
                && e.get("bot_id").is_none()
                && e["user"].is_string();
            if !user_message {
                return Ok(SlackInbound::Events(vec![]));
            }
            let ts = e["ts"].as_str().unwrap_or_default();
            Ok(SlackInbound::Events(vec![ChatEvent {
                id: v["event_id"].as_str().map(String::from).unwrap_or_else(|| format!("msg-{ts}")),
                channel: e["channel"].as_str().unwrap_or_default().to_string(),
                thread_ts: e["thread_ts"].as_str().filter(|t| *t != ts).map(String::from),
                user: e["user"].as_str().unwrap_or_default().to_string(),
                text: e["text"].as_str().unwrap_or_default().to_string(),
                action: None,
            }]))
        }
        _ => Ok(SlackInbound::Events(vec![])),
    }
}

fn parse_interaction(v: &Value) -> Vec<ChatEvent> {
    if v["type"] != "block_actions" {
        return vec![];
    }
    let channel = v["channel"]["id"].as_str().or(v["container"]["channel_id"].as_str()).unwrap_or_default();
    let thread_ts = v["message"]["thread_ts"]
        .as_str()
        .or(v["container"]["thread_ts"].as_str())
        .filter(|t| Some(*t) != v["message"]["ts"].as_str())
        .map(String::from);
    let user = v["user"]["id"].as_str().unwrap_or_default();
    let mut out = vec![];
    for a in v["actions"].as_array().into_iter().flatten() {
        let value = a["value"].as_str().unwrap_or_default();
        let (decision, qid) = if let Some(q) = value.strip_prefix("approve:") {
            ("approved", q)
        } else if let Some(q) = value.strip_prefix("reject:") {
            ("rejected", q)
        } else {
            continue;
        };
        let at = a["action_ts"].as_str().or(v["trigger_id"].as_str()).unwrap_or_default();
        out.push(ChatEvent {
            id: format!("action-{at}-{user}"),
            channel: channel.to_string(),
            thread_ts: thread_ts.clone(),
            user: user.to_string(),
            text: String::new(),
            action: Some((qid.to_string(), decision.to_string())),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::{HeaderValue, Uri};

    #[derive(Clone, Debug)]
    struct Rec {
        method: String,
        path: String,
        query: HashMap<String, String>,
        auth: String,
        body: Value,
    }
    type Handler = Arc<dyn Fn(&Rec) -> Value + Send + Sync>;

    async fn mock(h: Handler) -> (String, Arc<Mutex<Vec<Rec>>>) {
        let log: Arc<Mutex<Vec<Rec>>> = Arc::default();
        let log2 = log.clone();
        let app = axum::Router::new().fallback(move |method: axum::http::Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let h = h.clone();
            let log = log2.clone();
            async move {
                let rec = Rec {
                    method: method.to_string(),
                    path: uri.path().to_string(),
                    query: url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect(),
                    auth: headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string(),
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                let v = h(&rec);
                log.lock().unwrap().push(rec);
                axum::Json(v)
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://127.0.0.1:{port}/api"), log)
    }

    fn slack(api: &str) -> Slack {
        Slack::from_loaded(&Loaded {
            row: ConnectorRow { id: Uuid::nil(), kind: "slack".into(), role: "chat".into(), name: "slack".into(), config: json!({"api_url": api}), service_identity: None },
            secrets: HashMap::from([("bot_token".to_string(), "xoxb-test".to_string())]),
        })
        .unwrap()
    }

    fn channels_handler(r: &Rec) -> Option<Value> {
        (r.path == "/api/conversations.list").then(|| {
            if r.query.get("cursor").map(String::as_str) == Some("page2") {
                json!({"ok": true, "channels": [{"id": "C0FACTORY1", "name": "factory"}], "response_metadata": {"next_cursor": ""}})
            } else {
                json!({"ok": true, "channels": [{"id": "C0GENERAL1", "name": "general"}], "response_metadata": {"next_cursor": "page2"}})
            }
        })
    }

    #[tokio::test]
    async fn post_resolves_channel_and_sends_metadata_mention_and_buttons() {
        let h: Handler = Arc::new(|r| {
            channels_handler(r).unwrap_or_else(|| match r.path.as_str() {
                "/api/chat.postMessage" => json!({"ok": true, "channel": r.body["channel"], "ts": "1767600000.000200", "message": {}}),
                _ => json!({"ok": false, "error": "unknown_method"}),
            })
        });
        let (api, log) = mock(h).await;
        let s = slack(&api);
        let parent = ChatRef { channel: "C0FACTORY1".into(), ts: "1767600000.000100".into(), thread_ts: None };
        let msg = ChatMessage { text: "Approve the plan?".into(), approval_for: Some("q-123".into()), marker: Some("chatpost:r1:plan".into()), mention: Some("U0ANN".into()) };
        let r = s.post("#factory", Some(&parent), &msg).await.unwrap();
        assert_eq!(r, ChatRef { channel: "C0FACTORY1".into(), ts: "1767600000.000200".into(), thread_ts: Some("1767600000.000100".into()) });
        // Second post: channel id comes from the cache; no mention, no buttons, top level.
        let r2 = s.post("factory", None, &ChatMessage { text: "hi".into(), ..Default::default() }).await.unwrap();
        assert_eq!(r2.thread_ts, None);

        let log = log.lock().unwrap();
        assert!(log.iter().all(|r| r.auth == "Bearer xoxb-test"));
        assert_eq!(log.iter().filter(|r| r.path == "/api/conversations.list").count(), 2, "two pages, then cached");
        let lists: Vec<&Rec> = log.iter().filter(|r| r.path == "/api/conversations.list").collect();
        assert_eq!(lists[0].query["types"], "public_channel,private_channel");
        assert_eq!(lists[1].query["cursor"], "page2");
        let posts: Vec<&Rec> = log.iter().filter(|r| r.path == "/api/chat.postMessage").collect();
        let b = &posts[0].body;
        assert_eq!(posts[0].method, "POST");
        assert_eq!(b["channel"], "C0FACTORY1");
        assert_eq!(b["thread_ts"], "1767600000.000100");
        assert_eq!(b["text"], "<@U0ANN> Approve the plan?");
        assert_eq!(b["metadata"], json!({"event_type": "factory_marker", "event_payload": {"marker": "chatpost:r1:plan"}}));
        assert_eq!(b["blocks"][0]["text"]["text"], "<@U0ANN> Approve the plan?");
        let els = b["blocks"][1]["elements"].as_array().unwrap();
        assert_eq!(els[0]["value"], "approve:q-123");
        assert_eq!(els[0]["style"], "primary");
        assert_eq!(els[1]["value"], "reject:q-123");
        let b2 = &posts[1].body;
        assert!(b2.get("blocks").is_none() && b2.get("metadata").is_none() && b2.get("thread_ts").is_none());
        assert_eq!(b2["text"], "hi");
    }

    #[tokio::test]
    async fn update_and_errors() {
        let h: Handler = Arc::new(|r| match r.path.as_str() {
            "/api/chat.update" => json!({"ok": true, "channel": r.body["channel"], "ts": r.body["ts"]}),
            "/api/auth.test" => json!({"ok": false, "error": "invalid_auth"}),
            _ => json!({"ok": false, "error": "channel_not_found"}),
        });
        let (api, log) = mock(h).await;
        let s = slack(&api);
        s.update(&ChatRef { channel: "C0FACTORY1".into(), ts: "1.2".into(), thread_ts: None }, "Approved by ann").await.unwrap();
        let body = log.lock().unwrap()[0].body.clone();
        assert_eq!(body, json!({"channel": "C0FACTORY1", "ts": "1.2", "text": "Approved by ann", "blocks": []}));
        let e = s.test_connection().await.unwrap_err().to_string();
        assert!(e.contains("invalid_auth"), "{e}");
    }

    #[tokio::test]
    async fn find_by_marker_searches_history_and_threads() {
        let h: Handler = Arc::new(|r| {
            channels_handler(r).unwrap_or_else(|| match r.path.as_str() {
                "/api/conversations.history" => json!({"ok": true, "has_more": false, "messages": [
                    {"type": "message", "ts": "300.1", "text": "other", "metadata": {"event_type": "factory_marker", "event_payload": {"marker": "x"}}},
                    {"type": "message", "ts": "200.1", "thread_ts": "200.1", "reply_count": 2, "text": "Run started",
                        "metadata": {"event_type": "factory_marker", "event_payload": {"marker": "thread:r1"}}},
                    {"type": "message", "ts": "100.1", "text": "no metadata"}
                ]}),
                "/api/conversations.replies" => json!({"ok": true, "has_more": false, "messages": [
                    {"type": "message", "ts": "200.1", "thread_ts": "200.1", "text": "Run started"},
                    {"type": "message", "ts": "200.5", "thread_ts": "200.1", "text": "plan", "metadata": {"event_type": "factory_marker", "event_payload": {"marker": "chatpost:r1:plan"}}}
                ]}),
                _ => json!({"ok": false, "error": "unknown_method"}),
            })
        });
        let (api, log) = mock(h).await;
        let s = slack(&api);
        assert_eq!(
            s.find_by_marker("#factory", "thread:r1").await.unwrap(),
            Some(ChatRef { channel: "C0FACTORY1".into(), ts: "200.1".into(), thread_ts: None })
        );
        assert_eq!(
            s.find_by_marker("#factory", "chatpost:r1:plan").await.unwrap(),
            Some(ChatRef { channel: "C0FACTORY1".into(), ts: "200.5".into(), thread_ts: Some("200.1".into()) })
        );
        assert_eq!(s.find_by_marker("C0FACTORY1", "missing").await.unwrap(), None);
        let log = log.lock().unwrap();
        let hist = log.iter().find(|r| r.path == "/api/conversations.history").unwrap();
        assert_eq!(hist.method, "GET");
        assert_eq!(hist.query["channel"], "C0FACTORY1");
        assert_eq!(hist.query["include_all_metadata"], "true");
        let rep = log.iter().find(|r| r.path == "/api/conversations.replies").unwrap();
        assert_eq!(rep.query["ts"], "200.1");
        assert_eq!(rep.query["include_all_metadata"], "true");
    }

    #[tokio::test]
    async fn lookup_user_and_test_connection() {
        let h: Handler = Arc::new(|r| match r.path.as_str() {
            "/api/users.lookupByEmail" if r.query["email"] == "ann@acme.test" => json!({"ok": true, "user": {"id": "U0ANN", "name": "ann"}}),
            "/api/users.lookupByEmail" => json!({"ok": false, "error": "users_not_found"}),
            "/api/auth.test" => json!({"ok": true, "team": "Acme", "user": "factory", "user_id": "U0BOT", "team_id": "T1"}),
            _ => json!({"ok": false, "error": "unknown_method"}),
        });
        let (api, _log) = mock(h).await;
        let s = slack(&api);
        assert_eq!(s.lookup_user_by_email("ann@acme.test").await.unwrap().as_deref(), Some("U0ANN"));
        assert_eq!(s.lookup_user_by_email("nobody@acme.test").await.unwrap(), None);
        assert_eq!(s.test_connection().await.unwrap(), "connected to Slack workspace Acme as factory");
    }

    fn signed(secret: &str, ts: i64, body: &[u8]) -> HeaderMap {
        use hmac::{Hmac, KeyInit, Mac};
        let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("v0:{ts}:").as_bytes());
        mac.update(body);
        let mut h = HeaderMap::new();
        h.insert("x-slack-request-timestamp", HeaderValue::from_str(&ts.to_string()).unwrap());
        h.insert("x-slack-signature", HeaderValue::from_str(&format!("v0={}", hex::encode(mac.finalize().into_bytes()))).unwrap());
        h
    }

    #[test]
    fn signature_verification() {
        // Example from Slack's "Verifying requests from Slack" documentation.
        let secret = "8f742231b10e8888abcd99yyyzzz85a5";
        let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        let mut h = HeaderMap::new();
        h.insert("x-slack-request-timestamp", HeaderValue::from_static("1531420618"));
        h.insert("x-slack-signature", HeaderValue::from_static("v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503"));
        assert!(verify_signature_at(secret, &h, body, 1531420618 + 10));
        assert!(!verify_signature_at(secret, &h, body, 1531420618 + 301), "stale");
        assert!(!verify_signature_at(secret, &h, b"tampered", 1531420618));
        assert!(!verify_signature_at("wrong", &h, body, 1531420618));

        let now = chrono::Utc::now().timestamp();
        let b = br#"{"type":"event_callback"}"#;
        assert!(verify_signature("s3cret", &signed("s3cret", now, b), b));
        assert!(!verify_signature("s3cret", &signed("other", now, b), b));
        assert!(!verify_signature("s3cret", &signed("s3cret", now - 600, b), b), "replay outside window");
        assert!(!verify_signature("s3cret", &HeaderMap::new(), b));
    }

    #[test]
    fn webhook_parsing() {
        let json_h = {
            let mut h = HeaderMap::new();
            h.insert("content-type", HeaderValue::from_static("application/json"));
            h
        };
        let uv = json!({"token": "Jhj5dZrVaK7ZwHHjRyZWjbDl", "challenge": "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P", "type": "url_verification"});
        match parse_webhook(&json_h, uv.to_string().as_bytes()).unwrap() {
            SlackInbound::UrlVerification(c) => assert_eq!(c, "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"),
            other => panic!("{other:?}"),
        }

        let msg = |event: Value| {
            json!({
                "token": "XXYYZZ", "team_id": "T0001", "api_app_id": "A0001", "type": "event_callback",
                "event_id": "Ev0PV52K21", "event_time": 1767600001, "authorizations": [{"team_id": "T0001", "user_id": "U0BOT", "is_bot": true}],
                "event": event
            })
            .to_string()
        };
        let events = |s: String| match parse_webhook(&json_h, s.as_bytes()).unwrap() {
            SlackInbound::Events(e) => e,
            other => panic!("{other:?}"),
        };
        let e = events(msg(json!({"type": "message", "channel": "C0FACTORY1", "user": "U0ANN", "text": "approve",
            "ts": "1767600001.000300", "thread_ts": "1767600000.000100", "channel_type": "channel", "event_ts": "1767600001.000300"})));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].id, "Ev0PV52K21");
        assert_eq!(e[0].channel, "C0FACTORY1");
        assert_eq!(e[0].thread_ts.as_deref(), Some("1767600000.000100"));
        assert_eq!(e[0].user, "U0ANN");
        assert_eq!(e[0].text, "approve");
        assert!(e[0].action.is_none());
        let top = events(msg(json!({"type": "message", "channel": "C0FACTORY1", "user": "U0ANN", "text": "start ticket_build PROJ-1", "ts": "1767600002.000100"})));
        assert_eq!(top[0].thread_ts, None);
        assert!(events(msg(json!({"type": "message", "channel": "C0FACTORY1", "bot_id": "B01", "user": "U0BOT", "text": "Run started", "ts": "1.1"}))).is_empty());
        assert!(events(msg(json!({"type": "message", "subtype": "message_changed", "channel": "C0FACTORY1", "message": {"text": "x"}, "ts": "1.2"}))).is_empty());
        assert!(events(msg(json!({"type": "reaction_added", "user": "U0ANN", "reaction": "thumbsup"}))).is_empty());

        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U0ANN", "username": "ann", "name": "ann", "team_id": "T0001"},
            "api_app_id": "A0001", "token": "XXYYZZ",
            "container": {"type": "message", "message_ts": "1767600003.000100", "channel_id": "C0FACTORY1", "is_ephemeral": false, "thread_ts": "1767600000.000100"},
            "trigger_id": "123456.7890.abcdef",
            "team": {"id": "T0001", "domain": "acme"},
            "channel": {"id": "C0FACTORY1", "name": "factory"},
            "message": {"type": "message", "user": "U0BOT", "ts": "1767600003.000100", "thread_ts": "1767600000.000100", "text": "Approve the plan?"},
            "response_url": "https://hooks.slack.com/actions/T0001/1/abc",
            "actions": [{"action_id": "factory_reject", "block_id": "factory_approval:q-123", "text": {"type": "plain_text", "text": "Reject"},
                "value": "reject:q-123", "style": "danger", "type": "button", "action_ts": "1767600004.123456"}]
        });
        let form = format!("payload={}", url::form_urlencoded::byte_serialize(payload.to_string().as_bytes()).collect::<String>());
        let mut fh = HeaderMap::new();
        fh.insert("content-type", HeaderValue::from_static("application/x-www-form-urlencoded"));
        let SlackInbound::Events(e) = parse_webhook(&fh, form.as_bytes()).unwrap() else { panic!() };
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].action, Some(("q-123".to_string(), "rejected".to_string())));
        assert_eq!(e[0].user, "U0ANN");
        assert_eq!(e[0].channel, "C0FACTORY1");
        assert_eq!(e[0].thread_ts.as_deref(), Some("1767600000.000100"));
        assert_eq!(e[0].id, "action-1767600004.123456-U0ANN");
        let mut p2 = payload.clone();
        p2["actions"][0]["value"] = json!("approve:q-123");
        let form2 = format!("payload={}", url::form_urlencoded::byte_serialize(p2.to_string().as_bytes()).collect::<String>());
        let SlackInbound::Events(e) = parse_webhook(&HeaderMap::new(), form2.as_bytes()).unwrap() else { panic!() };
        assert_eq!(e[0].action, Some(("q-123".to_string(), "approved".to_string())));
        assert!(parse_webhook(&fh, b"foo=bar").is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(mention_prefix("U1"), "<@U1>");
        assert_eq!(mention_prefix("U1, U2"), "<@U1> <@U2>");
        assert_eq!(mention_prefix("<@U1>"), "<@U1>");
        assert!(is_channel_id("C0123ABCD"));
        assert!(!is_channel_id("factory"));
        assert!(!is_channel_id("#factory"));
    }

    #[tokio::test]
    #[ignore]
    async fn contract_post_find_update() {
        let get = |k: &str| std::env::var(format!("FACTORY_CONTRACT_SLACK_{k}")).ok().filter(|s| !s.is_empty());
        let (Some(token), Some(channel)) = (get("BOT_TOKEN"), get("CHANNEL")) else {
            eprintln!("skipping: set FACTORY_CONTRACT_SLACK_BOT_TOKEN and FACTORY_CONTRACT_SLACK_CHANNEL");
            return;
        };
        let s = Slack::from_loaded(&Loaded {
            row: ConnectorRow { id: Uuid::nil(), kind: "slack".into(), role: "chat".into(), name: "contract".into(), config: json!({}), service_identity: None },
            secrets: HashMap::from([("bot_token".to_string(), token)]),
        })
        .unwrap();
        println!("{}", s.test_connection().await.unwrap());
        let marker = format!("contract:{}", Uuid::now_v7());
        let top = s.post(&channel, None, &ChatMessage { text: "Contract test thread".into(), marker: Some(format!("{marker}:top")), ..Default::default() }).await.unwrap();
        let reply = s
            .post(&channel, Some(&top), &ChatMessage { text: "Contract approval".into(), approval_for: Some(Uuid::now_v7().to_string()), marker: Some(marker.clone()), mention: None })
            .await
            .unwrap();
        assert_eq!(reply.thread_ts.as_deref(), Some(top.ts.as_str()));
        assert_eq!(s.find_by_marker(&channel, &format!("{marker}:top")).await.unwrap(), Some(top.clone()));
        assert_eq!(s.find_by_marker(&channel, &marker).await.unwrap(), Some(reply.clone()));
        s.update(&reply, "Contract approval (decided)").await.unwrap();
        assert_eq!(s.lookup_user_by_email("nobody-factory-contract@example.invalid").await.unwrap(), None);
    }
}
