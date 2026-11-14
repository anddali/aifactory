//! Entra ID sign-in (OpenID Connect authorization code flow) with roles
//! mapped from Entra groups.
//!
//! The ID token comes straight from the token endpoint over TLS in exchange
//! for the code and the client secret, so (as OIDC Core 3.1.3.7 allows) its
//! issuer, audience, expiry and nonce are validated but its signature is not.
//! Group-to-role mappings are the `entra_group_roles` setting; grants from
//! Entra are replaced at every sign-in, manual grants are kept.

use crate::AppState;
use crate::api::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const COOKIE: &str = "factory_oidc";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

pub async fn login(State(state): State<AppState>) -> Response {
    let Some(e) = &state.cfg.entra else { return (StatusCode::NOT_FOUND, "Entra sign-in is not configured").into_response() };
    let st = Uuid::new_v4().simple().to_string();
    let nonce = Uuid::new_v4().simple().to_string();
    let sig = state.crypto.derive(&format!("oidc|{st}|{nonce}"));
    let url = format!(
        "{}/oauth2/v2.0/authorize?client_id={}&response_type=code&redirect_uri={}&response_mode=query&scope={}&state={st}&nonce={nonce}",
        e.authority.trim_end_matches('/'),
        e.client_id,
        url_escape(&e.redirect_url),
        url_escape("openid profile email"),
    );
    let mut r = Redirect::to(&url).into_response();
    r.headers_mut().insert("set-cookie", format!("{COOKIE}={st}.{nonce}.{sig}; Path=/auth; HttpOnly; SameSite=Lax; Max-Age=600").parse().unwrap());
    r
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn callback(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<Callback>) -> Response {
    match handle_callback(&state, &headers, q).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn handle_callback(state: &AppState, headers: &HeaderMap, q: Callback) -> ApiResult<Response> {
    let e = state.cfg.entra.clone().ok_or(ApiError(StatusCode::NOT_FOUND, "Entra sign-in is not configured".into()))?;
    if let Some(err) = q.error {
        return Err(ApiError(StatusCode::UNAUTHORIZED, format!("sign-in failed: {err}")));
    }
    let cookie = headers
        .get("cookie")
        .and_then(|h| h.to_str().ok())
        .and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix(&format!("{COOKIE}=")).map(String::from)))
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "sign-in session missing".into()))?;
    let parts: Vec<&str> = cookie.split('.').collect();
    if parts.len() != 3 || state.crypto.derive(&format!("oidc|{}|{}", parts[0], parts[1])) != parts[2] || q.state.as_deref() != Some(parts[0]) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "sign-in state mismatch".into()));
    }
    let nonce = parts[1];
    let code = q.code.ok_or(ApiError(StatusCode::BAD_REQUEST, "no code".into()))?;
    let resp = state
        .http
        .post(format!("{}/oauth2/v2.0/token", e.authority.trim_end_matches('/')))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", e.redirect_url.as_str()),
            ("client_id", e.client_id.as_str()),
            ("client_secret", e.client_secret.as_str()),
            ("scope", "openid profile email"),
        ])
        .send()
        .await
        .map_err(|x| ApiError(StatusCode::BAD_GATEWAY, format!("token endpoint: {x}")))?;
    let tok: Value = resp.json().await.map_err(|x| ApiError(StatusCode::BAD_GATEWAY, format!("token response: {x}")))?;
    let id_token = tok["id_token"].as_str().ok_or(ApiError(StatusCode::UNAUTHORIZED, "no id_token".into()))?;
    let claims = validate(id_token, &e.authority, &e.client_id, nonce).map_err(|m| ApiError(StatusCode::UNAUTHORIZED, m))?;
    let user = sign_in(state, &claims).await?;
    let mut r = crate::api::auth::start_session(state, user).await?;
    *r.status_mut() = StatusCode::FOUND;
    r.headers_mut().insert("location", "/".parse().unwrap());
    r.headers_mut().append("set-cookie", format!("{COOKIE}=; Path=/auth; Max-Age=0").parse().unwrap());
    Ok(r)
}

/// Check issuer, audience, expiry and nonce of an ID token's claims.
pub fn validate(id_token: &str, authority: &str, client_id: &str, nonce: &str) -> Result<Value, String> {
    let payload = id_token.split('.').nth(1).ok_or("malformed id_token")?;
    let claims: Value = serde_json::from_slice(&b64().decode(payload).map_err(|_| "bad id_token encoding")?).map_err(|_| "bad id_token json")?;
    let iss = claims["iss"].as_str().unwrap_or("");
    if iss.trim_end_matches('/') != format!("{}/v2.0", authority.trim_end_matches('/')) {
        return Err(format!("unexpected issuer {iss}"));
    }
    let aud_ok = match &claims["aud"] {
        Value::String(a) => a == client_id,
        Value::Array(a) => a.iter().any(|x| x == client_id),
        _ => false,
    };
    if !aud_ok {
        return Err("id_token audience mismatch".into());
    }
    if claims["exp"].as_i64().unwrap_or(0) < chrono::Utc::now().timestamp() {
        return Err("id_token expired".into());
    }
    if claims["nonce"].as_str() != Some(nonce) {
        return Err("nonce mismatch".into());
    }
    Ok(claims)
}

async fn sign_in(state: &AppState, claims: &Value) -> ApiResult<Uuid> {
    let email = claims["email"].as_str().or_else(|| claims["preferred_username"].as_str()).ok_or(ApiError(StatusCode::UNAUTHORIZED, "no email in token".into()))?.to_lowercase();
    let oid = claims["oid"].as_str().unwrap_or("").to_string();
    let name = claims["name"].as_str().unwrap_or("").to_string();
    let id: Uuid = sqlx::query_scalar("INSERT INTO users(id, email, display_name) VALUES ($1,$2,$3) ON CONFLICT (email) DO UPDATE SET display_name = COALESCE(NULLIF(EXCLUDED.display_name,''), users.display_name) RETURNING id")
        .bind(Uuid::new_v4())
        .bind(&email)
        .bind(&name)
        .fetch_one(&state.pool)
        .await?;
    // Identity mapping: link the Entra object id (and the email) to the user.
    for (p, ext) in [("entra", oid.as_str()), ("email", email.as_str())] {
        if !ext.is_empty() {
            sqlx::query("INSERT INTO identities(user_id, provider, external_id) VALUES ($1,$2,$3) ON CONFLICT (provider, external_id) DO UPDATE SET user_id=EXCLUDED.user_id")
                .bind(id)
                .bind(p)
                .bind(ext)
                .execute(&state.pool)
                .await?;
        }
    }
    let groups: Vec<String> = claims["groups"].as_array().map(|a| a.iter().filter_map(|g| g.as_str().map(String::from)).collect()).unwrap_or_default();
    let s = crate::settings::get(&state.pool).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM role_grants WHERE user_id=$1 AND source='entra'").bind(id).execute(&mut *tx).await?;
    let mut granted = vec![];
    for m in &s.entra_group_roles {
        if groups.contains(&m.group) {
            if m.role == "administer" && m.project_id.is_none() {
                sqlx::query("UPDATE users SET is_global_admin=true WHERE id=$1").bind(id).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO role_grants(user_id, project_id, role, source) VALUES ($1,$2,$3,'entra')").bind(id).bind(m.project_id).bind(&m.role).execute(&mut *tx).await?;
            granted.push(json!({"group": m.group, "project_id": m.project_id, "role": m.role}));
        }
    }
    tx.commit().await?;
    crate::audit::record(&state.pool, &email, Some(id), "auth.entra_login", None, None, json!({"groups": groups.len(), "grants": granted})).await;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Fake Entra (development and tests only)
// ---------------------------------------------------------------------------

pub mod fake {
    use super::*;
    use crate::connectors::fake::{store_get, store_put};
    use axum::Json;
    use axum::extract::Path;

    #[derive(Deserialize)]
    pub struct AuthQ {
        redirect_uri: String,
        state: String,
        nonce: String,
        client_id: String,
    }

    /// Signs in as the user set with `POST /api/fake/entra/user` and redirects back with a code.
    pub async fn authorize(State(st): State<AppState>, Path(tenant): Path<String>, Query(q): Query<AuthQ>) -> Response {
        let user: Value = store_get(&st.pool, "fake_entra", "user").await.ok().flatten().unwrap_or(json!({"email": "entra.user@example.test", "oid": "oid-1", "groups": []}));
        let code = Uuid::new_v4().simple().to_string();
        let authority = st.cfg.entra.as_ref().map(|e| e.authority.clone()).unwrap_or_else(|| format!("{}/fake-entra/{tenant}", st.cfg.public_urls[0]));
        let claims = json!({
            "iss": format!("{authority}/v2.0"), "aud": q.client_id, "exp": chrono::Utc::now().timestamp() + 600,
            "nonce": q.nonce, "email": user["email"], "oid": user["oid"], "name": user["name"], "groups": user["groups"],
        });
        let _ = store_put(&st.pool, "fake_entra_codes", &code, &claims).await;
        Redirect::to(&format!("{}?code={code}&state={}", q.redirect_uri, q.state)).into_response()
    }

    pub async fn token(State(st): State<AppState>, body: String) -> Response {
        let form: std::collections::HashMap<String, String> = body
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), urlish_decode(v)))
            .collect();
        if form.get("client_secret").map(|s| s.as_str()) != Some("fake-entra-secret") {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "invalid_client"}))).into_response();
        }
        let code = form.get("code").cloned().unwrap_or_default();
        let Ok(Some(claims)) = store_get::<Value>(&st.pool, "fake_entra_codes", &code).await else {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
        };
        let jwt = format!("{}.{}.", b64().encode(br#"{"alg":"none","typ":"JWT"}"#), b64().encode(claims.to_string()));
        Json(json!({"id_token": jwt, "token_type": "Bearer", "expires_in": 600})).into_response()
    }

    fn urlish_decode(s: &str) -> String {
        let s = s.replace('+', " ");
        let b = s.as_bytes();
        let mut out = vec![];
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub async fn set_user(State(st): State<AppState>, _u: crate::api::auth::User, Json(v): Json<Value>) -> ApiResult<Json<Value>> {
        store_put(&st.pool, "fake_entra", "user", &v).await?;
        Ok(Json(json!({})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tok(c: Value) -> String {
        format!("x.{}.", b64().encode(c.to_string()))
    }
    #[test]
    fn validates_claims() {
        let a = "https://login.example/t1";
        let ok = json!({"iss": "https://login.example/t1/v2.0", "aud": "cid", "exp": chrono::Utc::now().timestamp() + 60, "nonce": "n"});
        assert!(validate(&tok(ok.clone()), a, "cid", "n").is_ok());
        let mut bad = ok.clone();
        bad["aud"] = json!("other");
        assert!(validate(&tok(bad), a, "cid", "n").is_err());
        let mut bad = ok.clone();
        bad["nonce"] = json!("m");
        assert!(validate(&tok(bad), a, "cid", "n").is_err());
        let mut bad = ok.clone();
        bad["exp"] = json!(1);
        assert!(validate(&tok(bad), a, "cid", "n").is_err());
        let mut bad = ok;
        bad["iss"] = json!("https://evil/v2.0");
        assert!(validate(&tok(bad), a, "cid", "n").is_err());
    }
}
