//! Prometheus metrics.

use prometheus::{
    Encoder, IntCounterVec, IntGaugeVec, TextEncoder, register_int_counter_vec, register_int_gauge_vec,
};
use std::sync::LazyLock;

pub static EVENTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("factory_events_total", "Run events appended", &["family"]).unwrap()
});
pub static PRIMITIVES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("factory_primitive_calls_total", "Primitive calls", &["name", "outcome"]).unwrap()
});
pub static JOBS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("factory_jobs_total", "Jobs processed", &["kind", "outcome"]).unwrap()
});
pub static RUNNERS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("factory_runner_launches_total", "Runner launches", &["launcher"]).unwrap()
});
pub static RUNS_BY_STATE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!("factory_runs", "Runs by state", &["state"]).unwrap()
});
pub static POLICY_DENIALS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("factory_policy_denials_total", "Refused actions", &["rule"]).unwrap()
});

pub fn render() -> String {
    // Touch statics so they appear even before first use.
    let _ = (&*EVENTS, &*PRIMITIVES, &*JOBS, &*RUNNERS, &*RUNS_BY_STATE, &*POLICY_DENIALS);
    let mut buf = vec![];
    TextEncoder::new().encode(&prometheus::gather(), &mut buf).ok();
    String::from_utf8(buf).unwrap_or_default()
}
