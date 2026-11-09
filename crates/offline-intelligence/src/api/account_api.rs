//! Account API — e-mail verified sign-up, sign-in with user name and password,
//! and sign-in on a new computer.
//!
//! How it works, and what it does not do:
//!
//! * Joining (and signing in on a computer that has not seen the account
//!   before) always goes through a six-digit one-time code sent to the e-mail
//!   address. The code proves the person owns the address. Nothing is written
//!   to the account until the code is entered: the details wait, with the
//!   password already hashed, in `login_otps`.
//! * After that, this computer signs in with the user name (or e-mail) and
//!   password, with no e-mail involved.
//! * Accounts live in this computer's own database. The app is offline, so
//!   there is no shared account server: another computer does not know the
//!   account, which is exactly why it asks for a code the first time.
//! * E-mail goes out over SMTP using SMTP_HOST / SMTP_PORT / SMTP_USER /
//!   SMTP_PASS (and optionally SMTP_FROM) from the environment. Nothing here
//!   ships credentials; without them the endpoints say mail is not set up.

use crate::api::auth_api::{create_jwt_token, get_auth_state, hash_password, verify_password};
use crate::shared_state::UnifiedAppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{error, info, warn};

const OTP_TTL_SECS: i64 = 10 * 60;
const OTP_RESEND_SECS: i64 = 30;
const OTP_MAX_ATTEMPTS: i64 = 5;
const SIGNIN_MAX_FAILS: u32 = 5;
const SIGNIN_WINDOW_SECS: i64 = 5 * 60;

/// Failed password attempts per identifier: (count, window start).
static SIGNIN_FAILS: Mutex<Option<HashMap<String, (u32, i64)>>> = Mutex::new(None);

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub username: String,
    pub password: String,
    pub age: i64,
    pub dob: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub email: String,
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct SignInRequest {
    pub identifier: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    first_name: String,
    last_name: String,
    username: String,
    password_hash: String,
    age: i64,
    dob: String,
}

fn fail(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(json!({ "success": false, "message": message }))).into_response()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn valid_email(e: &str) -> bool {
    let mut parts = e.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(l), Some(d), None) => !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.') && !e.contains(' '),
        _ => false,
    }
}

fn valid_username(u: &str) -> bool {
    (3..=32).contains(&u.len()) && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn code_hash(email: &str, code: &str, secret: &str) -> String {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.update(email.to_lowercase().as_bytes());
    h.update(code.as_bytes());
    hex::encode(h.finalize())
}


/// A mail setting: the environment first, then `mail.env` in the app's data
/// folder (KEY=value lines). The file is deliberately not part of the project
/// or the installer, so the mailbox password is never shipped with the app.
pub(crate) fn mail_setting(key: &str) -> String {
    if let Ok(v) = std::env::var(key) {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    let path = dirs::data_local_dir().map(|d| d.join("Offline Counsel AI").join("mail.env"));
    if let Some(Ok(text)) = path.map(std::fs::read_to_string) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == key {
                    return v.trim().trim_matches('"').to_string();
                }
            }
        }
    }
    String::new()
}

pub(crate) fn smtp_configured() -> bool {
    !mail_setting("SMTP_USER").is_empty() && !mail_setting("SMTP_PASS").is_empty()
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Send one HTML e-mail from the configured address. Blocking SMTP runs off
/// the async threads.
async fn send_mail(to_email: &str, to_name: &str, subject: &str, html: String) -> Result<(), String> {
    send_mail_reply(to_email, to_name, subject, html, None).await
}

/// `send_mail` with an optional Reply-To, so a reply goes to the person the
/// message is about (used for feedback).
pub(crate) async fn send_mail_reply(
    to_email: &str,
    to_name: &str,
    subject: &str,
    html: String,
    reply_to: Option<String>,
) -> Result<(), String> {
    let to_email = to_email.to_string();
    let to_name = to_name.to_string();
    let subject = subject.to_string();
    tokio::task::spawn_blocking(move || {
        use lettre::{
            message::header::ContentType, transport::smtp::authentication::Credentials, Message, SmtpTransport, Transport,
        };
        let user = mail_setting("SMTP_USER");
        let pass = mail_setting("SMTP_PASS");
        if user.is_empty() || pass.is_empty() {
            return Err("Mail is not set up on this computer.".to_string());
        }
        let host = Some(mail_setting("SMTP_HOST")).filter(|h| !h.is_empty()).unwrap_or_else(|| "smtp.hostinger.com".to_string());
        let port: u16 = mail_setting("SMTP_PORT").parse().unwrap_or(587);
        let from_addr = Some(mail_setting("SMTP_FROM")).filter(|f| !f.is_empty()).unwrap_or_else(|| user.clone());

        let mut builder = Message::builder()
            .from(
                format!("Offline Counsel AI <{}>", from_addr)
                    .parse()
                    .map_err(|e: lettre::address::AddressError| e.to_string())?,
            )
            .to(format!("{} <{}>", to_name, to_email)
                .parse()
                .map_err(|e: lettre::address::AddressError| e.to_string())?)
            .subject(subject);
        if let Some(addr) = reply_to.and_then(|r| r.parse().ok()) {
            builder = builder.reply_to(addr);
        }
        let message = builder
            .header(ContentType::TEXT_HTML)
            .body(html)
            .map_err(|e| e.to_string())?;

        let mailer = SmtpTransport::starttls_relay(&host)
            .map_err(|e| format!("Could not reach the mail server: {}", e))?
            .port(port)
            .credentials(Credentials::new(user, pass))
            .build();
        mailer.send(&message).map_err(|e| format!("Could not send the e-mail: {}", e))?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn otp_email(name: &str, code: &str) -> String {
    format!(
        r#"<div style="font-family:-apple-system,'Segoe UI',Roboto,sans-serif;max-width:480px;margin:0 auto;padding:24px;color:#111">
<h2 style="margin:0 0 12px">Your Offline Counsel AI code</h2>
<p>Hi {},</p>
<p>Enter this code in the app to verify your e-mail address:</p>
<p style="font-size:34px;letter-spacing:8px;font-weight:700;margin:20px 0">{}</p>
<p style="color:#666;font-size:13px">It expires in 10 minutes. If you did not ask for it, you can ignore this e-mail.</p>
</div>"#,
        escape(name),
        code
    )
}

fn welcome_email(name: &str) -> String {
    format!(
        r#"<div style="font-family:-apple-system,'Segoe UI',Roboto,sans-serif;max-width:520px;margin:0 auto;padding:24px;color:#111">
<h2 style="margin:0 0 12px">Thank you for downloading Offline Counsel AI</h2>
<p>Hi {},</p>
<p>Your e-mail is verified and your account is ready. Thank you for downloading and using Offline Counsel AI.</p>
<p>We will send product updates to this address from product@offlineintelligence.io. You can reply to any of them to reach us.</p>
<p style="color:#666;font-size:13px">Your documents and conversations stay on your computer.</p>
</div>"#,
        escape(name)
    )
}

/// GET /auth/status — whether this computer has an account yet, and whether
/// mail can be sent from it.
pub async fn status(State(state): State<UnifiedAppState>) -> impl IntoResponse {
    let auth = get_auth_state(&state);
    let has_users = auth.users.count_users().map(|n| n > 0).unwrap_or(false);
    Json(json!({ "has_users": has_users, "email_configured": smtp_configured() }))
}

/// POST /auth/register/start — validate the details and e-mail a code.
pub async fn register_start(State(state): State<UnifiedAppState>, Json(p): Json<StartRequest>) -> axum::response::Response {
    let auth = get_auth_state(&state);
    let email = p.email.trim().to_lowercase();
    let first = p.first_name.trim().to_string();
    let last = p.last_name.trim().to_string();
    let username = p.username.trim().to_string();

    if first.is_empty() || last.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "Enter your first and last name.");
    }
    if !valid_email(&email) {
        return fail(StatusCode::BAD_REQUEST, "Enter a valid e-mail address.");
    }
    if !valid_username(&username) {
        return fail(StatusCode::BAD_REQUEST, "User names are 3-32 letters, numbers, dots, dashes or underscores.");
    }
    if p.password.chars().count() < 8 {
        return fail(StatusCode::BAD_REQUEST, "Use a password of at least 8 characters.");
    }
    // The age comes from the date of birth, computed here; `p.age` is ignored.
    let age = match crate::api::age_gate::check(&p.dob) {
        crate::api::age_gate::Check::Ok(a) => a,
        crate::api::age_gate::Check::Invalid => return fail(StatusCode::BAD_REQUEST, "Enter a valid date of birth."),
        _ => return not_eligible(),
    };
    if !smtp_configured() {
        return fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "E-mail is not set up on this computer, so a verification code cannot be sent. Add the SMTP settings to the app's .env file.",
        );
    }
    match auth.users.username_taken(&username, &email) {
        Ok(true) => return fail(StatusCode::CONFLICT, "That user name is taken. Choose another."),
        Ok(false) => {}
        Err(e) => {
            error!("username check failed: {}", e);
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.");
        }
    }

    // One code per address every 30 seconds.
    if let Ok(Some((_, _, _, created, _))) = auth.users.get_otp(&email) {
        if now() - created < OTP_RESEND_SECS {
            return fail(StatusCode::TOO_MANY_REQUESTS, "A code was just sent. Wait a few seconds before asking again.");
        }
    }

    let password_hash = match hash_password(&p.password) {
        Ok(h) => h,
        Err(e) => {
            error!("{}", e);
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.");
        }
    };
    let code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000));
    let pending = Pending { first_name: first.clone(), last_name: last, username, password_hash, age, dob: p.dob };
    let t = now();
    if let Err(e) = auth.users.save_otp(
        &email,
        &code_hash(&email, &code, &auth.jwt_secret),
        &serde_json::to_string(&pending).unwrap_or_default(),
        t,
        t + OTP_TTL_SECS,
    ) {
        error!("saving code failed: {}", e);
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.");
    }

    match send_mail(&email, &first, "Your Offline Counsel AI verification code", otp_email(&first, &code)).await {
        Ok(()) => {
            info!("Verification code sent to {}", email);
            Json(json!({ "success": true, "message": "We sent a 6-digit code to your e-mail." })).into_response()
        }
        Err(e) => {
            warn!("verification e-mail failed: {}", e);
            let _ = auth.users.clear_otp(&email);
            fail(StatusCode::BAD_GATEWAY, &e)
        }
    }
}

/// POST /auth/register/verify — check the code, create the account, sign in.
pub async fn register_verify(State(state): State<UnifiedAppState>, Json(p): Json<VerifyRequest>) -> axum::response::Response {
    let auth = get_auth_state(&state);
    let email = p.email.trim().to_lowercase();
    let code = p.code.trim().to_string();

    let (hash, payload, attempts, _created, expires) = match auth.users.get_otp(&email) {
        Ok(Some(row)) => row,
        _ => return fail(StatusCode::BAD_REQUEST, "No code is waiting for this e-mail. Ask for a new one."),
    };
    if now() > expires {
        let _ = auth.users.clear_otp(&email);
        return fail(StatusCode::BAD_REQUEST, "That code has expired. Ask for a new one.");
    }
    if attempts >= OTP_MAX_ATTEMPTS {
        let _ = auth.users.clear_otp(&email);
        return fail(StatusCode::TOO_MANY_REQUESTS, "Too many wrong codes. Ask for a new one.");
    }
    if code_hash(&email, &code, &auth.jwt_secret) != hash {
        let _ = auth.users.bump_otp_attempts(&email);
        return fail(StatusCode::UNAUTHORIZED, "That code is not right.");
    }

    let pending: Pending = match serde_json::from_str(&payload) {
        Ok(p) => p,
        Err(_) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Ask for a new code."),
    };
    let display = format!("{} {}", pending.first_name, pending.last_name);
    let (id, is_new) = match auth.users.upsert_verified_user(
        &email,
        &display,
        &pending.username,
        &pending.password_hash,
        &pending.first_name,
        &pending.last_name,
        pending.age,
        &pending.dob,
    ) {
        Ok(r) => r,
        Err(e) => {
            error!("creating the account failed: {}", e);
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "Could not create the account. Try again.");
        }
    };
    let _ = auth.users.clear_otp(&email);
    let _ = auth.users.set_dob(id, &pending.dob, pending.age);

    if is_new {
        let (mail, name) = (email.clone(), pending.first_name.clone());
        tokio::spawn(async move {
            if let Err(e) = send_mail(&mail, &name, "Thank you for downloading Offline Counsel AI", welcome_email(&name)).await {
                warn!("welcome e-mail failed: {}", e);
            }
        });
    }

    session(auth.jwt_secret.as_str(), id, &email, &display, "You're in.")
}

/// POST /auth/signin — user name (or e-mail) and password.
pub async fn sign_in(State(state): State<UnifiedAppState>, Json(p): Json<SignInRequest>) -> axum::response::Response {
    let auth = get_auth_state(&state);
    let key = p.identifier.trim().to_lowercase();
    if key.is_empty() || p.password.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "Enter your user name and password.");
    }

    // Lock out a name after repeated wrong passwords.
    {
        let mut guard = SIGNIN_FAILS.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some((count, since)) = map.get(&key).copied() {
            if now() - since > SIGNIN_WINDOW_SECS {
                map.remove(&key);
            } else if count >= SIGNIN_MAX_FAILS {
                return fail(StatusCode::TOO_MANY_REQUESTS, "Too many attempts. Wait a few minutes and try again.");
            }
        }
    }

    let user = match auth.users.get_user_by_login(&key) {
        Ok(Some(u)) if u.email_verified => u,
        // The same answer for "no such account" and "wrong password".
        Ok(_) => return wrong_password(&key),
        Err(e) => {
            error!("sign-in lookup failed: {}", e);
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.");
        }
    };
    if !verify_password(&p.password, &user.password_hash).unwrap_or(false) {
        return wrong_password(&key);
    }
    if let Some(map) = SIGNIN_FAILS.lock().unwrap().as_mut() {
        map.remove(&key);
    }
    gated_session(&state, user.id, &user.email, &user.name, "Welcome back.")
}

fn wrong_password(key: &str) -> axum::response::Response {
    let mut guard = SIGNIN_FAILS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    let entry = map.entry(key.to_string()).or_insert((0, now()));
    entry.0 += 1;
    fail(StatusCode::UNAUTHORIZED, "That user name or password is not right.")
}

pub(crate) fn session(secret: &str, id: i64, email: &str, name: &str, message: &str) -> axum::response::Response {
    match create_jwt_token(email, name, secret) {
        Ok(token) => Json(json!({
            "success": true,
            "message": message,
            "token": token,
            "requires_verification": false,
            "user": { "id": id, "name": name, "email": email, "email_verified": true }
        }))
        .into_response(),
        Err(e) => {
            error!("{}", e);
            fail(StatusCode::INTERNAL_SERVER_ERROR, "Could not start a session.")
        }
    }
}


// ── Passwordless sign-in with a one-time code ──────────────────────────────
// Any e-mail provider (Gmail, Outlook, a firm's own domain): the code is the
// proof. An address with no account on this computer is told so, and the
// app moves to Create account with the address filled in.

const SIGNIN_PAYLOAD: &str = "signin";

#[derive(Debug, Deserialize)]
pub struct OtpStartRequest {
    pub email: String,
}

/// POST /auth/otp/start — e-mail a sign-in code to an existing account.
pub async fn otp_start(State(state): State<UnifiedAppState>, Json(p): Json<OtpStartRequest>) -> axum::response::Response {
    let auth = get_auth_state(&state);
    let email = p.email.trim().to_lowercase();
    if !valid_email(&email) {
        return fail(StatusCode::BAD_REQUEST, "Enter a valid e-mail address.");
    }
    if !smtp_configured() {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "E-mail is not set up on this computer, so a code cannot be sent.");
    }
    let user = match auth.users.get_user_by_login(&email) {
        Ok(Some(u)) if u.email_verified => u,
        Ok(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "success": false, "needs_account": true, "message": "There is no account for this e-mail on this computer yet." })),
            )
                .into_response()
        }
        Err(e) => {
            error!("otp lookup failed: {}", e);
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.");
        }
    };
    if let Ok(Some((_, _, _, created, _))) = auth.users.get_otp(&email) {
        if now() - created < OTP_RESEND_SECS {
            return fail(StatusCode::TOO_MANY_REQUESTS, "A code was just sent. Wait a few seconds before asking again.");
        }
    }
    let code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000));
    let t = now();
    if let Err(e) = auth.users.save_otp(&email, &code_hash(&email, &code, &auth.jwt_secret), SIGNIN_PAYLOAD, t, t + OTP_TTL_SECS) {
        error!("saving code failed: {}", e);
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.");
    }
    let first = user.name.split_whitespace().next().unwrap_or("there").to_string();
    match send_mail(&email, &first, "Your Offline Counsel AI sign-in code", otp_email(&first, &code)).await {
        Ok(()) => Json(json!({ "success": true, "message": "We sent a 6-digit code to your e-mail." })).into_response(),
        Err(e) => {
            warn!("sign-in code e-mail failed: {}", e);
            let _ = auth.users.clear_otp(&email);
            fail(StatusCode::BAD_GATEWAY, &e)
        }
    }
}

/// POST /auth/otp/verify — check a sign-in code and start the session.
pub async fn otp_verify(State(state): State<UnifiedAppState>, Json(p): Json<VerifyRequest>) -> axum::response::Response {
    let auth = get_auth_state(&state);
    let email = p.email.trim().to_lowercase();
    let (hash, payload, attempts, _created, expires) = match auth.users.get_otp(&email) {
        Ok(Some(row)) => row,
        _ => return fail(StatusCode::BAD_REQUEST, "No code is waiting for this e-mail. Ask for a new one."),
    };
    if payload != SIGNIN_PAYLOAD {
        return fail(StatusCode::BAD_REQUEST, "That code was sent to create an account. Finish creating it instead.");
    }
    if now() > expires {
        let _ = auth.users.clear_otp(&email);
        return fail(StatusCode::BAD_REQUEST, "That code has expired. Ask for a new one.");
    }
    if attempts >= OTP_MAX_ATTEMPTS {
        let _ = auth.users.clear_otp(&email);
        return fail(StatusCode::TOO_MANY_REQUESTS, "Too many wrong codes. Ask for a new one.");
    }
    if code_hash(&email, p.code.trim(), &auth.jwt_secret) != hash {
        let _ = auth.users.bump_otp_attempts(&email);
        return fail(StatusCode::UNAUTHORIZED, "That code is not right.");
    }
    let _ = auth.users.clear_otp(&email);
    match auth.users.get_user_by_login(&email) {
        Ok(Some(u)) => gated_session(&state, u.id, &u.email, &u.name, "Welcome back."),
        _ => fail(StatusCode::NOT_FOUND, "There is no account for this e-mail on this computer yet."),
    }
}


// ── Age check on every way in ──────────────────────────────────────────────

pub(crate) fn not_eligible() -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "success": false, "age_blocked": true, "message": crate::api::age_gate::NOT_ELIGIBLE })),
    )
        .into_response()
}

/// Who is waiting on a date of birth before they get a session: an existing
/// account without one, or a Google / Microsoft sign-up not yet created.
#[derive(Clone)]
pub(crate) enum AgeTicket {
    Existing { id: i64, email: String, name: String },
    NewSso { email: String, display: String, first: String, last: String },
}

const TICKET_TTL_SECS: i64 = 15 * 60;
static AGE_TICKETS: Mutex<Option<HashMap<String, (AgeTicket, i64)>>> = Mutex::new(None);

pub(crate) fn age_ticket(t: AgeTicket) -> String {
    let id = hex::encode(rand::thread_rng().gen::<[u8; 24]>());
    let mut g = AGE_TICKETS.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    map.retain(|_, (_, at)| now() - *at < TICKET_TTL_SECS);
    map.insert(id.clone(), (t, now()));
    id
}

pub(crate) fn needs_dob_json(ticket: &str) -> serde_json::Value {
    json!({ "success": true, "needs_dob": true, "ticket": ticket, "message": "Confirm your date of birth to continue." })
}

/// A session, but only for an adult with a confirmed date of birth.
pub(crate) fn gated_session(state: &UnifiedAppState, id: i64, email: &str, name: &str, message: &str) -> axum::response::Response {
    let auth = get_auth_state(state);
    match auth.users.get_dob(id).ok().flatten() {
        Some(dob) => match crate::api::age_gate::age_from(&dob) {
            Some(age) if age >= crate::api::age_gate::MIN_AGE => session(auth.jwt_secret.as_str(), id, email, name, message),
            _ => not_eligible(),
        },
        None => {
            if crate::api::age_gate::locked() {
                return not_eligible();
            }
            let t = age_ticket(AgeTicket::Existing { id, email: email.to_string(), name: name.to_string() });
            Json(needs_dob_json(&t)).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ConfirmAgeRequest {
    pub ticket: String,
    pub dob: String,
    /// The user ticked "this date of birth is accurate".
    pub attest: bool,
}

/// POST /auth/age — confirm a date of birth for a waiting sign-in.
pub async fn confirm_age(State(state): State<UnifiedAppState>, Json(p): Json<ConfirmAgeRequest>) -> axum::response::Response {
    if !p.attest {
        return fail(StatusCode::BAD_REQUEST, "Confirm that your date of birth is accurate.");
    }
    let ticket = {
        let g = AGE_TICKETS.lock().unwrap();
        g.as_ref().and_then(|m| m.get(&p.ticket)).filter(|(_, at)| now() - *at < TICKET_TTL_SECS).map(|(t, _)| t.clone())
    };
    let Some(ticket) = ticket else {
        return fail(StatusCode::BAD_REQUEST, "This sign-in has expired. Start again.");
    };
    let age = match crate::api::age_gate::check(&p.dob) {
        crate::api::age_gate::Check::Ok(a) => a,
        crate::api::age_gate::Check::Invalid => return fail(StatusCode::BAD_REQUEST, "Enter a valid date of birth."),
        _ => {
            if let Some(m) = AGE_TICKETS.lock().unwrap().as_mut() {
                m.remove(&p.ticket);
            }
            return not_eligible();
        }
    };
    if let Some(m) = AGE_TICKETS.lock().unwrap().as_mut() {
        m.remove(&p.ticket);
    }
    let auth = get_auth_state(&state);
    let (id, email, name) = match ticket {
        AgeTicket::Existing { id, email, name } => (id, email, name),
        AgeTicket::NewSso { email, display, first, last } => {
            match crate::api::oauth_api::create_sso_account(&state, &email, &display, &first, &last, age, &p.dob) {
                Ok(id) => (id, email, display),
                Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, &e),
            }
        }
    };
    if let Err(e) = auth.users.set_dob(id, &p.dob, age) {
        error!("saving date of birth failed: {}", e);
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.");
    }
    session(auth.jwt_secret.as_str(), id, &email, &name, "You're in.")
}
