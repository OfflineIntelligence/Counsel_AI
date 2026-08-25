//! Authentication API - User registration, login, and email verification
use crate::memory_db::UsersStore;
use crate::shared_state::UnifiedAppState;
use argon2::{password_hash::SaltString, Argon2, PasswordHasher, PasswordVerifier};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, TokenData, Validation};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct AuthState {
    pub users: UsersStore,
    pub jwt_secret: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub email: String,
    pub name: String,
    pub exp: i64,
    pub iat: i64,
}

#[derive(Debug, Deserialize)]
pub struct SignupRequest {
    pub name: String,
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyEmailRequest {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct MeRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub success: bool,
    pub message: String,
    pub user: Option<UserResponse>,
    pub token: Option<String>,
    pub requires_verification: bool,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub email_verified: bool,
}

const JWT_EXPIRY_HOURS: i64 = 24 * 7;

fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut rand::thread_rng());
    let argon2 = Argon2::default();
    
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("Failed to hash password: {}", e))?;
    
    Ok(hash.to_string())
}

fn verify_password(password: &str, hash: &str) -> Result<bool, String> {
    use argon2::PasswordHash;
    
    let parsed_hash = PasswordHash::new(hash)
        .map_err(|e| format!("Invalid hash: {}", e))?;
    
    match Argon2::default().verify_password(password.as_bytes(), &parsed_hash) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn generate_verification_token() -> String {
    let random: [u8; 32] = rand::thread_rng().gen();
    hex::encode(random)
}

fn create_jwt_token(email: &str, name: &str, secret: &str) -> Result<String, String> {
    let now = chrono::Utc::now().timestamp();
    let exp = now + (JWT_EXPIRY_HOURS * 3600);
    
    let claims = Claims {
        sub: email.to_string(),
        email: email.to_string(),
        name: name.to_string(),
        exp,
        iat: now,
    };
    
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| format!("Failed to create token: {}", e))
}

fn decode_jwt_token(token: &str, secret: &str) -> Result<TokenData<Claims>, String> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| format!("Invalid token: {}", e))
}

async fn send_verification_email(email: &str, name: &str, token: &str) -> Result<(), String> {
    use lettre::{
        Message, SmtpTransport, Transport,
        message::header::ContentType,
        transport::smtp::authentication::Credentials,
    };

    let smtp_user = std::env::var("SMTP_USER").unwrap_or_default();
    let smtp_pass = std::env::var("SMTP_PASS").unwrap_or_default();

    if smtp_user.is_empty() || smtp_pass.is_empty() {
        return Err("SMTP credentials not configured".into());
    }

    let smtp_host = std::env::var("SMTP_HOST").unwrap_or_else(|_| "smtp.gmail.com".to_string());
    let smtp_port: u16 = std::env::var("SMTP_PORT")
        .unwrap_or_else(|_| "587".to_string())
        .parse()
        .unwrap_or(587);

    let app_url = std::env::var("APP_URL").unwrap_or_else(|_| "offline-intelligence.io".to_string());
    let verify_link = format!("{}://{}/verify-email?token={}",
        if smtp_port == 465 { "https" } else { "http" },
        app_url,
        token
    );

    let from_address = format!("Offline_Intelligence <{}>", smtp_user);

    let email_msg = Message::builder()
        .from(from_address.parse().map_err(|e: lettre::address::AddressError| e.to_string())?)
        .to(format!("{} <{}>", name, email).parse().map_err(|e: lettre::address::AddressError| e.to_string())?)
        .subject("Verify your Offline_Intelligence account")
        .header(ContentType::TEXT_HTML)
        .body(format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="background: linear-gradient(135deg, #667eea 0%, #764ba2 100%); padding: 30px; border-radius: 10px 10px 0 0;">
        <h1 style="color: white; margin: 0; font-size: 28px;">Welcome to Offline_Intelligence!</h1>
    </div>
    <div style="background: #f9f9f9; padding: 30px; border-radius: 0 0 10px 10px;">
        <p>Hi {},</p>
        <p>Thank you for signing up for Offline_Intelligence! To get started, please verify your email address by clicking the button below:</p>
        <div style="text-align: center; margin: 30px 0;">
            <a href="{}" style="background: #667eea; color: white; padding: 14px 28px; text-decoration: none; border-radius: 8px; font-weight: 600; display: inline-block;">Verify Email Address</a>
        </div>
        <p style="color: #666; font-size: 14px;">Or copy and paste this link into your browser:</p>
        <p style="color: #667eea; word-break: break-all; font-size: 13px;">{}</p>
        <p style="color: #666; font-size: 14px;">This link will expire in 7 days.</p>
        <hr style="border: none; border-top: 1px solid #ddd; margin: 20px 0;">
        <p style="color: #999; font-size: 12px; margin: 0;">If you didn't create an account, you can safely ignore this email.</p>
    </div>
</body>
</html>"#,
            name, verify_link, verify_link
        )).map_err(|e| e.to_string())?;

    let creds = Credentials::new(smtp_user, smtp_pass);

    let mailer = SmtpTransport::starttls_relay(&smtp_host)
        .map_err(|e| format!("Failed to create SMTP transport: {}", e))?
        .port(smtp_port)
        .credentials(creds)
        .build();

    mailer.send(&email_msg)
        .map_err(|e| format!("Failed to send email: {}", e))?;

    Ok(())
}

fn get_auth_state(state: &UnifiedAppState) -> &AuthState {
    state.auth_state.as_ref().expect("Auth state not initialized").as_ref()
}

pub async fn signup(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<SignupRequest>,
) -> impl IntoResponse {
    let auth_state = get_auth_state(&state);
    let name = payload.name.trim();
    let email = payload.email.trim().to_lowercase();
    let password = payload.password;

    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(AuthResponse {
                success: false,
                message: "Name is required".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    if email.is_empty() || !email.contains('@') {
        return (
            StatusCode::BAD_REQUEST,
            Json(AuthResponse {
                success: false,
                message: "Valid email is required".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    if password.len() < 6 {
        return (
            StatusCode::BAD_REQUEST,
            Json(AuthResponse {
                success: false,
                message: "Password must be at least 6 characters".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    if let Ok(true) = auth_state.users.email_exists(&email) {
        return (
            StatusCode::CONFLICT,
            Json(AuthResponse {
                success: false,
                message: "An account with this email already exists".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    let password_hash = match hash_password(&password) {
        Ok(hash) => hash,
        Err(e) => {
            error!("Failed to hash password: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Failed to create account".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            );
        }
    };

    let verification_token = generate_verification_token();

    let email_result = send_verification_email(&email, &name, &verification_token).await;
    let email_ok = email_result.is_ok();
    
    if let Err(e) = email_result {
        warn!("Failed to send verification email: {}", e);
    }

    match auth_state.users.create_user(&email, &name, &password_hash, &verification_token) {
        Ok(user_id) => {
            info!("User created with id: {}", user_id);

            (
                StatusCode::CREATED,
                Json(AuthResponse {
                    success: true,
                    message: if email_ok {
                        "Account created! Please check your email to verify your account.".to_string()
                    } else {
                        "Account created but verification email failed to send. Please contact support.".to_string()
                    },
                    user: Some(UserResponse {
                        id: user_id,
                        name: name.to_string(),
                        email: email.clone(),
                        email_verified: false,
                    }),
                    token: None,
                    requires_verification: true,
                }),
            )
        }
        Err(e) => {
            error!("Failed to create user: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Failed to create account".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            )
        }
    }
}

pub async fn login(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    let auth_state = get_auth_state(&state);
    let email = payload.email.trim().to_lowercase();
    let password = payload.password;

    if email.is_empty() || password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(AuthResponse {
                success: false,
                message: "Email and password are required".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    let user = match auth_state.users.get_user_by_email(&email) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(AuthResponse {
                    success: false,
                    message: "Invalid email or password".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            )
        }
        Err(e) => {
            error!("Failed to get user: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Login failed".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            );
        }
    };

    if !user.email_verified {
        return (
            StatusCode::FORBIDDEN,
            Json(AuthResponse {
                success: false,
                message: "Please verify your email address before logging in".to_string(),
                user: Some(UserResponse {
                    id: user.id,
                    name: user.name,
                    email: user.email,
                    email_verified: false,
                }),
                token: None,
                requires_verification: true,
            }),
        );
    }

    let password_valid = match verify_password(&password, &user.password_hash) {
        Ok(valid) => valid,
        Err(e) => {
            error!("Failed to verify password: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Login failed".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            );
        }
    };

    if !password_valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(AuthResponse {
                success: false,
                message: "Invalid email or password".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    let token = match create_jwt_token(&email, &user.name, &auth_state.jwt_secret) {
        Ok(token) => token,
        Err(e) => {
            error!("Failed to create JWT token: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Login failed".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            );
        }
    };

    info!("User logged in: {}", email);

    (
        StatusCode::OK,
        Json(AuthResponse {
            success: true,
            message: "Login successful".to_string(),
            user: Some(UserResponse {
                id: user.id,
                name: user.name,
                email: user.email,
                email_verified: user.email_verified,
            }),
            token: Some(token),
            requires_verification: false,
        }),
    )
}

pub async fn verify_email(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<VerifyEmailRequest>,
) -> impl IntoResponse {
    let auth_state = get_auth_state(&state);
    let token = payload.token.trim();

    if token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(AuthResponse {
                success: false,
                message: "Verification token is required".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    let user = match auth_state.users.verify_email(token) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AuthResponse {
                    success: false,
                    message: "Invalid or expired verification token".to_string(),
                    user: None,
                    token: None,
                    requires_verification: true,
                }),
            )
        }
        Err(e) => {
            error!("Failed to verify email: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Email verification failed".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            );
        }
    };

    let token = match create_jwt_token(&user.email, &user.name, &auth_state.jwt_secret) {
        Ok(token) => token,
        Err(e) => {
            error!("Failed to create JWT token: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Verification succeeded but login failed".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            );
        }
    };

    info!("Email verified for user: {}", user.email);

    (
        StatusCode::OK,
        Json(AuthResponse {
            success: true,
            message: "Email verified successfully!".to_string(),
            user: Some(UserResponse {
                id: user.id,
                name: user.name,
                email: user.email,
                email_verified: true,
            }),
            token: Some(token),
            requires_verification: false,
        }),
    )
}

pub async fn get_current_user(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<MeRequest>,
) -> impl IntoResponse {
    let auth_state = get_auth_state(&state);
    let token = payload.token;

    if token.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(AuthResponse {
                success: false,
                message: "No token provided".to_string(),
                user: None,
                token: None,
                requires_verification: false,
            }),
        );
    }

    let token_data = match decode_jwt_token(&token, &auth_state.jwt_secret) {
        Ok(data) => data,
        Err(e) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(AuthResponse {
                    success: false,
                    message: format!("Invalid token: {}", e),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            )
        }
    };

    let user = match auth_state.users.get_user_by_email(&token_data.claims.email) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(AuthResponse {
                    success: false,
                    message: "User not found".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            )
        }
        Err(e) => {
            error!("Failed to get user: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthResponse {
                    success: false,
                    message: "Failed to get user info".to_string(),
                    user: None,
                    token: None,
                    requires_verification: false,
                }),
            );
        }
    };

    (
        StatusCode::OK,
        Json(AuthResponse {
            success: true,
            message: "User found".to_string(),
            user: Some(UserResponse {
                id: user.id,
                name: user.name,
                email: user.email,
                email_verified: user.email_verified,
            }),
            token: Some(token),
            requires_verification: false,
        }),
    )
}
