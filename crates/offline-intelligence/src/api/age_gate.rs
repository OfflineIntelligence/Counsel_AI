//! Age check: Offline Counsel AI is for adults (18 and over), the line the
//! professional AI tools draw (Claude, ChatGPT Enterprise, Harvey, Legora).
//!
//! How it follows the common guidance (FTC COPPA FAQ on "neutral age
//! screens", app-store age-rating rules, GDPR Art. 8):
//!   * The date of birth is asked neutrally: the form does not say where the
//!     line is, and nothing is pre-filled.
//!   * The SERVER computes the age from the date of birth. A client-sent
//!     "age" is never trusted.
//!   * A failed check locks this install for 24 hours, so the date cannot
//!     simply be changed and resubmitted (the "back button" problem). The
//!     app also remembers it locally.
//!   * Every account must have a confirmed date of birth before it gets a
//!     session — including Google / Microsoft sign-ups, which do not supply
//!     one, and accounts made before this check existed.
//!   * The confirmation time is kept (`age_verified_at`); the date of birth
//!     never leaves this computer.

use chrono::{Datelike, NaiveDate};

pub const MIN_AGE: i64 = 18;
const LOCK_SECS: i64 = 24 * 60 * 60;

/// The neutral refusal shown when the check fails (no hint of the line).
pub const NOT_ELIGIBLE: &str = "Sorry, you’re not eligible to use Offline Counsel AI.";

/// Age in whole years today, or None for an impossible date.
pub fn age_from(dob: &str) -> Option<i64> {
    let born = NaiveDate::parse_from_str(dob.trim(), "%Y-%m-%d").ok()?;
    let today = chrono::Utc::now().date_naive();
    if born >= today {
        return None;
    }
    let mut age = (today.year() - born.year()) as i64;
    if (today.month(), today.day()) < (born.month(), born.day()) {
        age -= 1;
    }
    (0..=120).contains(&age).then_some(age)
}

pub enum Check {
    Ok(i64),
    Invalid,
    Underage,
    Locked,
}

fn lock_file() -> Option<std::path::PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Offline Counsel AI").join("age.lock"))
}

/// Whether a failed check has locked this install.
pub fn locked() -> bool {
    let Some(path) = lock_file() else { return false };
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let at: i64 = text.trim().parse().unwrap_or(0);
    chrono::Utc::now().timestamp() - at < LOCK_SECS
}

fn lock() {
    if let Some(path) = lock_file() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, chrono::Utc::now().timestamp().to_string());
    }
}

/// Check a date of birth; an underage result locks this install.
pub fn check(dob: &str) -> Check {
    if locked() {
        return Check::Locked;
    }
    match age_from(dob) {
        None => Check::Invalid,
        Some(age) if age < MIN_AGE => {
            lock();
            tracing::info!("Age check failed; sign-up locked on this computer for 24 hours");
            Check::Underage
        }
        Some(age) => Check::Ok(age),
    }
}
