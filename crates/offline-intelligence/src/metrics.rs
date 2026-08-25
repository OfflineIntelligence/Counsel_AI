// Offline_Intelligence/_Server/metrics.rs
// Lock-free metrics using atomic operations and OnceCell

use prometheus::{Encoder, TextEncoder, Registry, IntCounterVec, IntGauge, Histogram};
use lazy_static::lazy_static;
use std::sync::OnceLock;
use axum::response::IntoResponse;
use axum::http::StatusCode;

lazy_static! {
    static ref REGISTRY: Registry = Registry::new();
}

static REQ_COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
static ACTIVE_SESSIONS: OnceLock<IntGauge> = OnceLock::new();
static QUEUE_DEPTH: OnceLock<IntGauge> = OnceLock::new();
static QUEUE_WAIT_TIME: OnceLock<Histogram> = OnceLock::new();

pub fn init_metrics() {
    // Initialize metrics once
    let req_counter = REQ_COUNTER.get_or_init(|| {
        IntCounterVec::new(
            prometheus::opts!("requests_total", "Total requests per route"),
            &["route", "status"]
        ).unwrap()
    });
    
    let active_sessions = ACTIVE_SESSIONS.get_or_init(|| {
        IntGauge::new("active_sessions", "Active streaming sessions").unwrap()
    });
    
    let queue_depth = QUEUE_DEPTH.get_or_init(|| {
        IntGauge::new("queue_depth", "Number of requests waiting in queue").unwrap()
    });
    
    let queue_wait_time = QUEUE_WAIT_TIME.get_or_init(|| {
        Histogram::with_opts(prometheus::HistogramOpts::new(
            "queue_wait_time_seconds",
            "Time spent waiting in queue"
        )).unwrap()
    });

    REGISTRY.register(Box::new(req_counter.clone())).ok();
    REGISTRY.register(Box::new(active_sessions.clone())).ok();
    REGISTRY.register(Box::new(queue_depth.clone())).ok();
    REGISTRY.register(Box::new(queue_wait_time.clone())).ok();
}

// Lock-free metric operations
pub fn inc_request(route: &str, status: &str) {
    if let Some(counter) = REQ_COUNTER.get() {
        counter.with_label_values(&[route, status]).inc();
    }
}

pub fn inc_sessions() {
    if let Some(gauge) = ACTIVE_SESSIONS.get() {
        gauge.inc();
    }
}

pub fn dec_sessions() {
    if let Some(gauge) = ACTIVE_SESSIONS.get() {
        gauge.dec();
    }
}

pub fn inc_queue() {
    if let Some(gauge) = QUEUE_DEPTH.get() {
        gauge.inc();
    }
}

pub fn dec_queue() {
    if let Some(gauge) = QUEUE_DEPTH.get() {
        gauge.dec();
    }
}

pub fn observe_queue_wait(duration: f64) {
    if let Some(histogram) = QUEUE_WAIT_TIME.get() {
        histogram.observe(duration);
    }
}

pub async fn get_metrics() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = REGISTRY.gather();
    let mut buffer = vec![];
    encoder.encode(&metric_families, &mut buffer).unwrap();
    
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        buffer,
    )
}
