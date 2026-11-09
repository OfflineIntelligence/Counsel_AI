//! Sign in with Google or Microsoft (Outlook, Hotmail, Microsoft 365).
//!
//! The desktop flow from RFC 8252: authorization code with PKCE, through the
//! system browser, back to a loopback redirect on this backend.
//!
//!   1. The app calls POST /auth/oauth/start; the backend makes a `state`
//!      and a PKCE verifier, keeps them in memory, and returns the provider's
//!      sign-in URL. The app opens it in the user's browser.
//!   2. The provider redirects to GET /auth/oauth/callback on this backend
//!      (127.0.0.1 / localhost, whatever port it listens on). The backend
//!      trades the code for tokens, reads the verified e-mail from the
//!      provider's userinfo endpoint, finds or creates the account, and
//!      stores the session against `state`.
//!   3. The app polls GET /auth/oauth/result?state=… until it is done.
//!
//! Client ids come from the environment or `mail.env` (same as SMTP):
//!   GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET (a "Desktop app" OAuth client;
//!   Google requires its secret even though it is not secret for desktop apps)
//!   MICROSOFT_CLIENT_ID (a public client with http://localhost redirect),
//!   MICROSOFT_TENANT (optional, default "common").
//! Without them the provider's button says it is not set up.

use crate::api::account_api::mail_setting;
use crate::api::auth_api::{get_auth_state, hash_password};
use crate::shared_state::UnifiedAppState;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{error, info, warn};

const FLOW_TTL_SECS: i64 = 10 * 60;

struct Flow {
    provider: &'static str,
    verifier: String,
    redirect_uri: String,
    created: i64,
    /// None while waiting; then the session JSON or an error message.
    result: Option<Result<Value, String>>,
}

static FLOWS: Mutex<Option<HashMap<String, Flow>>> = Mutex::new(None);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn random_token(bytes: usize) -> String {
    let buf: Vec<u8> = (0..bytes).map(|_| rand::thread_rng().gen()).collect();
    URL_SAFE_NO_PAD.encode(buf)
}

struct ProviderCfg {
    id: &'static str,
    name: &'static str,
    auth_url: String,
    token_url: String,
    userinfo_url: &'static str,
    scope: &'static str,
    client_id: String,
    client_secret: Option<String>,
}

fn provider(id: &str) -> Option<ProviderCfg> {
    match id {
        "google" => {
            let client_id = mail_setting("GOOGLE_CLIENT_ID");
            if client_id.is_empty() {
                return None;
            }
            Some(ProviderCfg {
                id: "google",
                name: "Google",
                auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token_url: "https://oauth2.googleapis.com/token".into(),
                userinfo_url: "https://openidconnect.googleapis.com/v1/userinfo",
                scope: "openid email profile",
                client_id,
                client_secret: Some(mail_setting("GOOGLE_CLIENT_SECRET")).filter(|s| !s.is_empty()),
            })
        }
        "microsoft" => {
            let client_id = mail_setting("MICROSOFT_CLIENT_ID");
            if client_id.is_empty() {
                return None;
            }
            let tenant = Some(mail_setting("MICROSOFT_TENANT")).filter(|t| !t.is_empty()).unwrap_or_else(|| "common".into());
            Some(ProviderCfg {
                id: "microsoft",
                name: "Microsoft",
                auth_url: format!("https://login.microsoftonline.com/{}/oauth2/v2.0/authorize", tenant),
                token_url: format!("https://login.microsoftonline.com/{}/oauth2/v2.0/token", tenant),
                userinfo_url: "https://graph.microsoft.com/oidc/userinfo",
                scope: "openid email profile User.Read",
                client_id,
                client_secret: None,
            })
        }
        _ => None,
    }
}

/// GET /auth/providers — which sign-in methods this computer can offer.
pub async fn providers() -> impl IntoResponse {
    Json(json!({
        "google": provider("google").is_some(),
        "microsoft": provider("microsoft").is_some(),
        "email_code": crate::api::account_api::smtp_configured(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub provider: String,
}

/// POST /auth/oauth/start — the provider URL to open in the browser.
pub async fn start(headers: HeaderMap, Json(p): Json<StartRequest>) -> axum::response::Response {
    let Some(cfg) = provider(&p.provider) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "success": false, "message": format!("Sign-in with {} is not set up on this computer.", p.provider) })),
        )
            .into_response();
    };
    // Back to this backend, on the host and port the app reached it on.
    // Microsoft accepts any port on http://localhost; Google any loopback port.
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1")
        .replace("127.0.0.1", "localhost");
    let redirect_uri = format!("http://{}/auth/oauth/callback", host);

    let state = random_token(24);
    let verifier = random_token(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let url = format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256&prompt=select_account",
        cfg.auth_url,
        urlencoding(&cfg.client_id),
        urlencoding(&redirect_uri),
        urlencoding(cfg.scope),
        state,
        challenge
    );

    let mut guard = FLOWS.lock().unwrap();
    let flows = guard.get_or_insert_with(HashMap::new);
    flows.retain(|_, f| now() - f.created < FLOW_TTL_SECS);
    flows.insert(state.clone(), Flow { provider: cfg.id, verifier, redirect_uri, created: now(), result: None });
    Json(json!({ "success": true, "url": url, "state": state })).into_response()
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><title>{0}</title>
<body style="margin:0;height:100vh;display:grid;place-items:center;background:#0b0b10;color:#f5f5f7;font-family:-apple-system,'Segoe UI',Roboto,sans-serif">
<div style="text-align:center;max-width:420px;padding:24px"><div style="font-size:44px;line-height:1">△</div>
<h1 style="font-size:20px;margin:16px 0 8px">{0}</h1><p style="color:#a1a1a6;margin:0">{1}</p></div></body>"#,
        title, body
    ))
}

/// GET /auth/oauth/callback — the browser lands here after the provider.
pub async fn callback(State(state): State<UnifiedAppState>, Query(q): Query<CallbackQuery>) -> impl IntoResponse {
    let Some(flow_state) = q.state.clone() else {
        return page("Sign-in failed", "The sign-in link was incomplete. Return to the app and try again.");
    };
    let flow = {
        let guard = FLOWS.lock().unwrap();
        guard.as_ref().and_then(|m| m.get(&flow_state)).map(|f| (f.provider, f.verifier.clone(), f.redirect_uri.clone()))
    };
    let Some((provider_id, verifier, redirect_uri)) = flow else {
        return page("Sign-in expired", "This sign-in has expired. Return to the app and start again.");
    };

    let result = match (q.code, q.error) {
        (_, Some(err)) => Err(q.error_description.unwrap_or(err)),
        (Some(code), None) => complete(&state, provider_id, &code, &verifier, &redirect_uri).await,
        _ => Err("No code came back from the provider.".into()),
    };
    let ok = result.is_ok();
    if let Err(e) = &result {
        warn!("{} sign-in failed: {}", provider_id, e);
    }
    if let Some(f) = FLOWS.lock().unwrap().as_mut().and_then(|m| m.get_mut(&flow_state)) {
        f.result = Some(result);
    }
    if ok {
        page("You're signed in", "You can close this tab and return to Offline Counsel AI.")
    } else {
        page("Sign-in failed", "Return to Offline Counsel AI to see why and try again.")
    }
}

/// Trade the code, read the verified e-mail, find or create the account.
async fn complete(state: &UnifiedAppState, provider_id: &str, code: &str, verifier: &str, redirect_uri: &str) -> Result<Value, String> {
    let cfg = provider(provider_id).ok_or("This provider is no longer set up.")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;

    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", cfg.client_id.clone()),
        ("code_verifier", verifier.to_string()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let tokens: Value = client
        .post(&cfg.token_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("Could not reach {}: {}", cfg.name, e))?
        .json()
        .await
        .map_err(|e| format!("{} sent an unreadable reply: {}", cfg.name, e))?;
    let access = tokens
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| tokens.get("error_description").and_then(|d| d.as_str()).unwrap_or("No access token was returned.").to_string())?;

    let info: Value = client
        .get(cfg.userinfo_url)
        .bearer_auth(access)
        .send()
        .await
        .map_err(|e| format!("Could not read your {} profile: {}", cfg.name, e))?
        .json()
        .await
        .map_err(|e| format!("Could not read your {} profile: {}", cfg.name, e))?;

    let email = info
        .get("email")
        .and_then(|e| e.as_str())
        .or_else(|| info.get("preferred_username").and_then(|e| e.as_str()))
        .map(|e| e.trim().to_lowercase())
        .filter(|e| e.contains('@'))
        .ok_or("Your account did not share an e-mail address.")?;
    // Google says whether it has verified the address; Microsoft accounts are
    // verified by the directory that issued them.
    if info.get("email_verified").and_then(|v| v.as_bool()) == Some(false) {
        return Err("That e-mail address is not verified with the provider.".into());
    }
    let given = info.get("given_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let family = info.get("family_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let full = info
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} {}", given, family).trim().to_string());
    let display = if full.is_empty() { email.split('@').next().unwrap_or("").to_string() } else { full };

    let auth = get_auth_state(state);
    // An existing account signs in, through the age check.
    if let Ok(Some(u)) = auth.users.get_user_by_login(&email) {
        info!("{} sign-in for existing account {}", cfg.name, email);
        return response_json(crate::api::account_api::gated_session(state, u.id, &u.email, &u.name, "Welcome back.")).await;
    }
    // A new one is not created until the date of birth is confirmed.
    if crate::api::age_gate::locked() {
        return Err(crate::api::age_gate::NOT_ELIGIBLE.into());
    }
    let (first, last) = if !given.is_empty() {
        (given, family)
    } else {
        let mut parts = display.splitn(2, ' ');
        (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string())
    };
    let ticket = crate::api::account_api::age_ticket(crate::api::account_api::AgeTicket::NewSso {
        email: email.clone(),
        display,
        first,
        last,
    });
    Ok(crate::api::account_api::needs_dob_json(&ticket))
}

/// Create the account for a Google / Microsoft sign-up whose age is
/// confirmed: a unique user name from the address and a random password
/// (they sign in with the provider, or with an e-mailed code).
pub(crate) fn create_sso_account(
    state: &UnifiedAppState,
    email: &str,
    display: &str,
    first: &str,
    last: &str,
    age: i64,
    dob: &str,
) -> Result<i64, String> {
    let auth = get_auth_state(state);
    let base: String = email
        .split('@')
        .next()
        .unwrap_or("user")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(24)
        .collect();
    let base = if base.len() < 3 { format!("user{}", base) } else { base };
    let mut username = base.clone();
    let mut n = 1;
    while auth.users.username_taken(&username, email).unwrap_or(false) {
        n += 1;
        username = format!("{}{}", base, n);
    }
    let password_hash = hash_password(&random_token(32)).map_err(|e| e.to_string())?;
    let (id, _) = auth
        .users
        .upsert_verified_user(email, display, &username, &password_hash, first, last, age, dob)
        .map_err(|e| {
            error!("creating SSO account failed: {}", e);
            "Could not create the account.".to_string()
        })?;
    info!("SSO sign-up created account {}", email);
    Ok(id)
}

/// The JSON body of a response, or its message as the error.
async fn response_json(resp: axum::response::Response) -> Result<Value, String> {
    let ok = resp.status().is_success();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if ok {
        Ok(v)
    } else {
        Err(v.get("message").and_then(|m| m.as_str()).unwrap_or("Sign-in failed.").to_string())
    }
}

#[derive(Debug, Deserialize)]
pub struct ResultQuery {
    pub state: String,
}

/// GET /auth/oauth/result?state=… — pending, the session, or why it failed.
/// A finished flow is removed once read.
pub async fn result(Query(q): Query<ResultQuery>) -> impl IntoResponse {
    let mut guard = FLOWS.lock().unwrap();
    let Some(flows) = guard.as_mut() else {
        return Json(json!({ "status": "expired" }));
    };
    let finished = match flows.get(&q.state) {
        None => return Json(json!({ "status": "expired" })),
        Some(f) if f.result.is_none() => {
            if now() - f.created > FLOW_TTL_SECS {
                flows.remove(&q.state);
                return Json(json!({ "status": "expired" }));
            }
            return Json(json!({ "status": "pending" }));
        }
        Some(_) => flows.remove(&q.state).and_then(|f| f.result),
    };
    match finished {
        Some(Ok(session)) => Json(json!({ "status": "done", "session": session })),
        Some(Err(message)) => Json(json!({ "status": "error", "message": message })),
        None => Json(json!({ "status": "expired" })),
    }
}
