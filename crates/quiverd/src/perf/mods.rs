//! Mod metrics over JSON (SIGHT-HUD §3.6, phase 1): the perf hub's side of the mod
//! registry (`quiver_core::modsdk::ModMetricsSink`).
//!
//! The registry has already parsed `metrics` as `PerfMessage<serde_json::Value>`, run each
//! descriptor through quiver-perf's validation in the mod's namespace, numbered them, applied
//! the per-connection rate limits (≤ 15 `m`/s, ≤ 20 marks/s, 4 declarations/s) and checked
//! every sample against its descriptor (`check_sample`). Here the hub:
//!
//! - files a connection's metrics under the session it belongs to (its `QUIVER_SESSION`,
//!   else the product's running session) as producer `mod:<surface>`, ≤ 512 per session;
//! - binds the roles they claim (first claim wins);
//! - checks timestamps: QPC ticks (Wine: CLOCK_MONOTONIC_RAW / 100) outside
//!   `[session start − 1 s, now + 1 s]` are dropped for marks; samples are kept at their
//!   arrival time either way (they're ≤ 10 Hz values, and their own times only matter once
//!   rings and correlation arrive) and a bad time marks the producer's clock `suspect`.

use std::sync::{Arc, Weak};

use quiver_api::mods::ModSdkConnection;
use quiver_core::modsdk::{DeclaredMetric, ModMetricsSink, Sample};
use quiver_perf::clock::{WINE_QPC_FREQ, mono_raw_ns, ticks_to_ns};
use quiver_perf::descriptor::CheckedValue;

use super::PerfHub;

pub struct PerfModSink {
    hub: Weak<PerfHub>,
}

impl PerfModSink {
    pub fn new(hub: &Arc<PerfHub>) -> Arc<PerfModSink> {
        Arc::new(PerfModSink { hub: Arc::downgrade(hub) })
    }
}

fn producer_id(m: &ModSdkConnection) -> String {
    format!("mod:{}", m.surface)
}

/// A mod's QPC time as CLOCK_MONOTONIC_RAW ns; `None` for 0 (not given).
fn qpc_ns(t: i64) -> Option<i64> {
    (t != 0).then(|| ticks_to_ns(t, WINE_QPC_FREQ)).flatten()
}

impl ModMetricsSink for PerfModSink {
    fn declared(&self, m: &ModSdkConnection, metrics: &[DeclaredMetric]) {
        let Some(hub) = self.hub.upgrade() else { return };
        let pid = producer_id(m);
        let r = hub.with_session(m.session.as_deref(), m.product_id, |s| {
            s.mod_conns.entry(m.surface.clone()).or_default().insert(m.conn);
            let label = format!("{} mod", m.surface);
            s.store.producer(&pid, &label, true).info.detail = format!("{} (SDK {}, proto {})", m.module.as_deref().unwrap_or("?"), m.sdk, m.proto);
            let mut refused = 0;
            for d in metrics {
                // The registry checked the namespace; check again, cheaply, before trusting it.
                if !d.id.starts_with(&format!("{}.", m.surface)) || (!s.store.has_metric(&d.id) && !PerfHub::session_has_room(s)) {
                    refused += 1;
                    continue;
                }
                s.store.declare(&pid, d.def.clone());
            }
            if refused > 0 {
                s.store.producer(&pid, &label, true).info.rejected += refused;
            }
            (s.id.clone(), s.product_id())
        });
        match r {
            Some((sid, product)) => hub.catalog_changed(&sid, product),
            None => tracing::debug!(product_id = m.product_id, surface = %m.surface, "mod metrics for a game without a perf session"),
        }
    }

    fn samples(&self, m: &ModSdkConnection, at: i64, samples: &[Sample]) {
        let Some(hub) = self.hub.upgrade() else { return };
        let pid = producer_id(m);
        let now = mono_raw_ns();
        hub.with_session(m.session.as_deref(), m.product_id, |s| {
            if let Some(t) = qpc_ns(at)
                && (t < s.store.started_ns - 1_000_000_000 || t > now + 1_000_000_000)
                && let Some(p) = s.store.producer_mut(&pid)
            {
                p.info.clock = "suspect".into();
            }
            for smp in samples {
                if !smp.id.starts_with(&format!("{}.", m.surface)) {
                    continue;
                }
                match &smp.value {
                    CheckedValue::Gauge(v) => s.store.push_num(&smp.id, now, *v),
                    CheckedValue::Counter(n) => s.store.push_counter(&smp.id, now, *n),
                    CheckedValue::State(i) => s.store.push_state(&smp.id, now, *i),
                    CheckedValue::Text(t) => s.store.push_text(&smp.id, now, t.clone()),
                    // Distributions: kept for recordings in phase 2.
                    CheckedValue::Histogram(_) => {}
                }
            }
        });
    }

    fn mark(&self, m: &ModSdkConnection, metric: &DeclaredMetric, at: i64, dur: Option<i64>, label: Option<&str>) {
        let Some(hub) = self.hub.upgrade() else { return };
        let now = mono_raw_ns();
        let t = qpc_ns(at).unwrap_or(now);
        let dur = dur.and_then(|d| ticks_to_ns(d, WINE_QPC_FREQ));
        hub.with_session(m.session.as_deref(), m.product_id, |s| {
            if !s.store.push_mark(&metric.id, t, now, dur, label.map(str::to_owned))
                && let Some(p) = s.store.producer_mut(&producer_id(m))
            {
                p.info.clock = "suspect".into();
                p.info.rejected += 1;
            }
        });
    }

    fn disconnected(&self, m: &ModSdkConnection) {
        let Some(hub) = self.hub.upgrade() else { return };
        let pid = producer_id(m);
        let r = hub.with_session(m.session.as_deref(), m.product_id, |s| {
            let last = match s.mod_conns.get_mut(&m.surface) {
                Some(c) => {
                    c.remove(&m.conn);
                    c.is_empty()
                }
                None => true,
            };
            if last {
                s.store.disconnect(&pid);
            }
            (s.id.clone(), s.product_id())
        });
        if let Some((sid, product)) = r {
            hub.catalog_changed(&sid, product);
        }
    }
}
