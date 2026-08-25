// User Login Notification API: sends email notification when a new user logs in
use axum::{
    extract::Json,
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
pub struct LoginNotificationRequest {
    pub user_name: String,
    pub user_email: String,
}

#[derive(Debug, Serialize)]
pub struct LoginNotificationResponse {
    pub success: bool,
    pub message: String,
}

pub async fn notify_user_login(
    Json(payload): Json<LoginNotificationRequest>,
) -> (StatusCode, Json<LoginNotificationResponse>) {
    if payload.user_name.trim().is_empty() || payload.user_email.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(LoginNotificationResponse {
                success: false,
                message: "User name and email are required".to_string(),
            }),
        );
    }

    info!("Received login notification for user: {}", payload.user_email);

    // Try to send email notification (non-blocking — don't fail if email fails)
    match send_login_notification_email(&payload).await {
        Ok(user_number) => {
            info!("Login notification email #{} sent to product team for user: {}", user_number, payload.user_email);
            (
                StatusCode::OK,
                Json(LoginNotificationResponse {
                    success: true,
                    message: format!("Login notification sent. Welcome user #{}!", user_number),
                }),
            )
        }
        Err(e) => {
            warn!("Login notification email not sent (SMTP may not be configured): {}", e);
            // Still return success - we don't want to block login if email fails
            (
                StatusCode::OK,
                Json(LoginNotificationResponse {
                    success: true,
                    message: "Login successful. Email notification not sent.".to_string(),
                }),
            )
        }
    }
}

/// Get the next user number for sequential tracking
fn get_next_user_number() -> u64 {
    let counter_path = std::path::Path::new("./data/user_counter.txt");
    
    // Read current counter
    let current = if counter_path.exists() {
        std::fs::read_to_string(counter_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    
    let next = current + 1;
    
    // Save next counter
    if let Some(parent) = counter_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(counter_path, next.to_string());
    
    next
}

async fn send_login_notification_email(payload: &LoginNotificationRequest) -> Result<u64, Box<dyn std::error::Error>> {
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

    // Get sequential user number
    let user_number = get_next_user_number();

    let from_address = format!("Offline_Intelligence Login <{}>", smtp_user);

    let email = Message::builder()
        .from(from_address.parse()?)
        .to("Product Team <product@offlineintelligence.io>".parse()?)
        .subject(format!("USER #{} - New User Login", user_number))
        .header(ContentType::TEXT_PLAIN)
        .body(format!(
            "USER #{}\n\nA new user has logged in to Offline_Intelligence:\n\nName: {}\nEmail: {}\n\nTime: {}",
            user_number,
            payload.user_name,
            payload.user_email,
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        ))?;

    let creds = Credentials::new(smtp_user, smtp_pass);

    let mailer = SmtpTransport::starttls_relay(&smtp_host)?
        .port(smtp_port)
        .credentials(creds)
        .build();

    mailer.send(&email)?;

    Ok(user_number)
}
