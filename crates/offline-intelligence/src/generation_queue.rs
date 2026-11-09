//! Admission control for answer generation (`/generate/stream`).
//!
//! Driven by the .env keys that Config parses:
//!   MAX_CONCURRENT_STREAMS  answers generating at once (the shell sets 4);
//!   QUEUE_SIZE              answers allowed to wait for a free stream;
//!   QUEUE_TIMEOUT_SECONDS   how long one may wait before it is refused;
//!   REQUESTS_PER_SECOND     token bucket on new generation requests.
//!
//! Queue depth and wait time are reported through the Prometheus metrics
//! `queue_depth` and `queue_wait_time_seconds`, active answers through
//! `active_sessions`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

struct Queue {
    streams: Arc<Semaphore>,
    waiting: AtomicUsize,
    queue_size: usize,
    queue_timeout: Duration,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    rate: f64,
    last: Instant,
}

static QUEUE: OnceLock<Queue> = OnceLock::new();

/// Set the limits from Config. Called once at startup; later calls are ignored.
pub fn configure(cfg: &crate::config::Config) {
    let rate = cfg.requests_per_second.max(1) as f64;
    let _ = QUEUE.set(Queue {
        streams: Arc::new(Semaphore::new(cfg.max_concurrent_streams.max(1) as usize)),
        waiting: AtomicUsize::new(0),
        queue_size: cfg.queue_size as usize,
        queue_timeout: Duration::from_secs(cfg.queue_timeout_seconds.max(1)),
        bucket: Mutex::new(Bucket { tokens: rate, rate, last: Instant::now() }),
    });
}

fn queue() -> &'static Queue {
    QUEUE.get_or_init(|| Queue {
        streams: Arc::new(Semaphore::new(4)),
        waiting: AtomicUsize::new(0),
        queue_size: 100,
        queue_timeout: Duration::from_secs(30),
        bucket: Mutex::new(Bucket { tokens: 24.0, rate: 24.0, last: Instant::now() }),
    })
}

/// Why a request was not admitted.
#[derive(Debug, PartialEq)]
pub enum Refused {
    /// REQUESTS_PER_SECOND exceeded.
    RateLimited,
    /// QUEUE_SIZE requests are already waiting.
    QueueFull,
    /// Waited QUEUE_TIMEOUT_SECONDS without a free stream.
    TimedOut,
}

impl Refused {
    pub fn message(&self) -> &'static str {
        match self {
            Refused::RateLimited => "Too many requests at once. Please wait a moment and try again.",
            Refused::QueueFull => "Too many answers are waiting. Please wait for one to finish and try again.",
            Refused::TimedOut => "The model is busy with other answers. Please try again in a moment.",
        }
    }
}

/// Held for the whole life of one answer; frees the stream on drop.
pub struct StreamPermit {
    _permit: OwnedSemaphorePermit,
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        crate::metrics::dec_sessions();
    }
}

/// Wait for a free generation stream, within the configured limits.
pub async fn admit() -> Result<StreamPermit, Refused> {
    let q = queue();
    {
        let mut b = q.bucket.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * b.rate).min(b.rate);
        b.last = now;
        if b.tokens < 1.0 {
            return Err(Refused::RateLimited);
        }
        b.tokens -= 1.0;
    }
    if let Ok(p) = Arc::clone(&q.streams).try_acquire_owned() {
        crate::metrics::inc_sessions();
        return Ok(StreamPermit { _permit: p });
    }
    if q.waiting.fetch_add(1, Ordering::SeqCst) >= q.queue_size {
        q.waiting.fetch_sub(1, Ordering::SeqCst);
        return Err(Refused::QueueFull);
    }
    crate::metrics::inc_queue();
    let started = Instant::now();
    let got = tokio::time::timeout(q.queue_timeout, Arc::clone(&q.streams).acquire_owned()).await;
    q.waiting.fetch_sub(1, Ordering::SeqCst);
    crate::metrics::dec_queue();
    crate::metrics::observe_queue_wait(started.elapsed().as_secs_f64());
    match got {
        Ok(Ok(p)) => {
            crate::metrics::inc_sessions();
            Ok(StreamPermit { _permit: p })
        }
        _ => Err(Refused::TimedOut),
    }
}
