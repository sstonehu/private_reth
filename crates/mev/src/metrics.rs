use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

/// Per-method atomic counters for periodic delta reporting.
///
/// One instance per RPC method (`mev_eth_call`, `mev_debug_traceCall`, `mev_trace_call`).
#[derive(Default, Debug)]
pub struct MethodCounters {
    /// Total requests entering the mev_* interface.
    pub total: AtomicU64,
    /// Requests routed to the EVM worker pool (active epoch match).
    pub worker: AtomicU64,
    /// Requests degraded to native eth_call (stale block_id).
    pub degraded: AtomicU64,
    /// Worker execution errors (revert / halt / evm error).
    pub errors: AtomicU64,
}

impl MethodCounters {
    #[inline]
    pub fn inc_total(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn inc_worker(&self) {
        self.worker.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn inc_degraded(&self) {
        self.degraded.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn inc_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn load_all(&self) -> Snapshot {
        Snapshot {
            total: self.total.load(Ordering::Relaxed),
            worker: self.worker.load(Ordering::Relaxed),
            degraded: self.degraded.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    total: u64,
    worker: u64,
    degraded: u64,
    errors: u64,
}

impl Snapshot {
    fn delta(&self, prev: &Self) -> Self {
        Self {
            total: self.total.saturating_sub(prev.total),
            worker: self.worker.saturating_sub(prev.worker),
            degraded: self.degraded.saturating_sub(prev.degraded),
            errors: self.errors.saturating_sub(prev.errors),
        }
    }

    fn degraded_pct(&self) -> u64 {
        if self.total == 0 { 0 } else { self.degraded * 100 / self.total }
    }
}

/// Shared counters for all three mev_* methods.
#[derive(Default, Debug)]
pub struct MevCounters {
    pub eth_call: MethodCounters,
    pub debug_trace_call: MethodCounters,
    pub trace_call: MethodCounters,
}

impl MevCounters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

// ── Prometheus metric name constants ────────────────────────────────────────

const REQUESTS_TOTAL: &str = "mev_requests_total";
const WORKER_PATH_TOTAL: &str = "mev_worker_path_total";
const DEGRADED_PATH_TOTAL: &str = "mev_degraded_path_total";
const ERRORS_TOTAL: &str = "mev_errors_total";
const E2E_SECONDS: &str = "mev_e2e_duration_seconds";

// ── Per-request inline helpers ───────────────────────────────────────────────

/// Record a request entering a mev_* interface.
#[inline]
pub fn record_request(method: &'static str, c: &MethodCounters) {
    c.inc_total();
    metrics::counter!(REQUESTS_TOTAL, "method" => method).increment(1);
}

/// Record a request routed to the EVM worker pool.
#[inline]
pub fn record_worker_path(method: &'static str, c: &MethodCounters) {
    c.inc_worker();
    metrics::counter!(WORKER_PATH_TOTAL, "method" => method).increment(1);
}

/// Record a request degraded to the native eth_call fallback.
#[inline]
pub fn record_degraded_path(method: &'static str, c: &MethodCounters) {
    c.inc_degraded();
    metrics::counter!(DEGRADED_PATH_TOTAL, "method" => method).increment(1);
}

/// Record a worker-side execution error.
#[inline]
pub fn record_error(method: &'static str, kind: &'static str, c: &MethodCounters) {
    c.inc_error();
    metrics::counter!(ERRORS_TOTAL, "method" => method, "kind" => kind).increment(1);
}

/// Record end-to-end latency (API entry → result returned).
#[inline]
pub fn record_e2e_latency(method: &'static str, duration: Duration) {
    metrics::histogram!(E2E_SECONDS, "method" => method).record(duration.as_secs_f64());
}

// ── Method label constants ───────────────────────────────────────────────────

pub mod method {
    pub const ETH_CALL: &str = "eth_call";
    pub const DEBUG_TRACE: &str = "debug_traceCall";
    pub const TRACE_CALL: &str = "trace_call";
}

// ── Periodic tracing reporter ────────────────────────────────────────────────

/// Spawn a background task that emits a structured `tracing::info!` log every
/// `interval`, reporting cumulative totals and per-interval deltas for all three
/// mev_* methods.
///
/// Log format (structured fields, visible in JSON log exporters):
/// ```text
/// reth::mev::stats  mev periodic stats
///   eth_call.total=1234  eth_call.delta=56  eth_call.worker=50
///   eth_call.degraded=6  eth_call.degraded_pct=10  eth_call.errors=0
///   ...
/// ```
pub fn spawn_periodic_reporter(counters: Arc<MevCounters>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip the immediate first tick

        let mut prev_eth = Snapshot::default();
        let mut prev_dbg = Snapshot::default();
        let mut prev_trc = Snapshot::default();

        loop {
            ticker.tick().await;

            let cur_eth = counters.eth_call.load_all();
            let cur_dbg = counters.debug_trace_call.load_all();
            let cur_trc = counters.trace_call.load_all();

            let d_eth = cur_eth.delta(&prev_eth);
            let d_dbg = cur_dbg.delta(&prev_dbg);
            let d_trc = cur_trc.delta(&prev_trc);

            tracing::info!(
                target: "reth::mev::stats",
                // ── mev_eth_call ──────────────────────────────────────
                eth_call_total        = cur_eth.total,
                eth_call_delta        = d_eth.total,
                eth_call_worker       = d_eth.worker,
                eth_call_degraded     = d_eth.degraded,
                eth_call_degraded_pct = d_eth.degraded_pct(),
                eth_call_errors       = d_eth.errors,
                // ── mev_debug_traceCall ───────────────────────────────
                debug_trace_total        = cur_dbg.total,
                debug_trace_delta        = d_dbg.total,
                debug_trace_worker       = d_dbg.worker,
                debug_trace_degraded     = d_dbg.degraded,
                debug_trace_degraded_pct = d_dbg.degraded_pct(),
                debug_trace_errors       = d_dbg.errors,
                // ── mev_trace_call ────────────────────────────────────
                trace_call_total        = cur_trc.total,
                trace_call_delta        = d_trc.total,
                trace_call_worker       = d_trc.worker,
                trace_call_degraded     = d_trc.degraded,
                trace_call_degraded_pct = d_trc.degraded_pct(),
                trace_call_errors       = d_trc.errors,
                "mev periodic stats"
            );

            metrics::gauge!("mev_degraded_pct", "method" => method::ETH_CALL)
                .set(d_eth.degraded_pct() as f64);
            metrics::gauge!("mev_degraded_pct", "method" => method::DEBUG_TRACE)
                .set(d_dbg.degraded_pct() as f64);
            metrics::gauge!("mev_degraded_pct", "method" => method::TRACE_CALL)
                .set(d_trc.degraded_pct() as f64);

            prev_eth = cur_eth;
            prev_dbg = cur_dbg;
            prev_trc = cur_trc;
        }
    });
}
