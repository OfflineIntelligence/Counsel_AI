// Feedback API: receives user feedback, saves locally, and optionally emails admin
use axum::{
    extract::Json,
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::{info, error, warn};

#[derive(Debug, Deserialize)]
pub struct FeedbackRequest {
    pub message: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct FeedbackResponse {
    pub success: bool,
    pub message: String,
}

pub async fn submit_feedback(
    Json(payload): Json<FeedbackRequest>,
) -> (StatusCode, Json<FeedbackResponse>) {
    if payload.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(FeedbackResponse {
                success: false,
                message: "Feedback message cannot be empty".to_string(),
            }),
        );
    }

    info!("Received feedback submission ({} chars)", payload.message.len());

    // Always save feedback locally first
    if let Err(e) = save_feedback_locally(&payload) {
        error!("Failed to save feedback locally: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(FeedbackResponse {
                success: false,
                message: "Failed to save feedback. Please try again.".to_string(),
            }),
        );
    }

    info!("Feedback saved locally");

    // Try to send email if SMTP is configured (non-blocking — don't fail if email fails)
    match send_feedback_email(&payload).await {
        Ok(feedback_number) => info!("Feedback email #{} sent to product team", feedback_number),
        Err(e) => warn!("Email not sent (SMTP may not be configured): {}", e),
    }

    (
        StatusCode::OK,
        Json(FeedbackResponse {
            success: true,
            message: "Feedback submitted successfully. Thank you!".to_string(),
        }),
    )
}

fn save_feedback_locally(payload: &FeedbackRequest) -> Result<(), Box<dyn std::error::Error>> {
    let db_path = dirs::data_local_dir()
        .map(|d| d.join("Offline Counsel AI").join("data"))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"));
    let db_path = db_path.as_path();
    std::fs::create_dir_all(db_path)?;

    let mut conn = rusqlite::Connection::open(db_path.join("feedback.db"))?;
    crate::memory_db::prepare_connection(&mut conn)?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS feedback (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            message TEXT NOT NULL,
            email TEXT DEFAULT '',
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );"
    )?;

    conn.execute(
        "INSERT INTO feedback (message, email) VALUES (?1, ?2)",
        rusqlite::params![payload.message, payload.email],
    )?;

    Ok(())
}

/// Get the next feedback number for sequential tracking
fn get_next_feedback_number() -> u64 {
    // Resolve path relative to executable directory for production
    let base_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let counter_path = base_dir.join("data").join("feedback_counter.txt");
    
    // Read current counter
    let current = if counter_path.exists() {
        std::fs::read_to_string(&counter_path)
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

async fn send_feedback_email(payload: &FeedbackRequest) -> Result<u64, Box<dyn std::error::Error>> {
    use crate::api::account_api::{escape, send_mail_reply, smtp_configured};

    if !smtp_configured() {
        return Err("SMTP credentials not configured".into());
    }
    let feedback_number = get_next_feedback_number();
    let who = if payload.name.trim().is_empty() { "A user".to_string() } else { payload.name.trim().to_string() };
    let from_email = if payload.email.trim().is_empty() { "Not provided".to_string() } else { payload.email.trim().to_string() };
    let html = format!(
        r#"<div style="font-family:-apple-system,'Segoe UI',Roboto,sans-serif;max-width:600px;margin:0 auto;padding:24px;color:#111">
<h2 style="margin:0 0 4px">Feedback #{}</h2>
<p style="margin:0 0 16px;color:#666">From {} ({})</p>
<div style="white-space:pre-wrap;border:1px solid #e5e5e5;border-radius:12px;padding:16px">{}</div>
</div>"#,
        feedback_number,
        escape(&who),
        escape(&from_email),
        escape(&payload.message)
    );
    let reply_to = if payload.email.trim().is_empty() { None } else { Some(payload.email.trim().to_string()) };
    send_mail_reply(
        "product@offlineintelligence.io",
        "Product Team",
        &format!("FEEDBACK #{} - Offline Counsel AI", feedback_number),
        html,
        reply_to,
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    Ok(feedback_number)
}
