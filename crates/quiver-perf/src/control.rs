//! The perf messages of the mod pipe's protocol 2 (SIGHT-HUD §3.6), as serde types. The pipe
//! envelope is MOD-CONFIG's: a JSON object whose `t` field names the message; unknown `t`
//! values and unknown fields are ignored both ways. [`PerfMessage`] covers only the perf
//! messages; `hello.caps` and `welcome.perf` are fields of the options protocol's own
//! messages ([`CAP_METRICS`], [`PerfWelcome`]).
//!
//! One change from the §3.6 table: `m` and `mark` carry their QPC timestamp as `ts`, because
//! `t` is the envelope's message type.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::descriptor::{Clock, MetricDescriptor, SampleValue};

/// `hello.caps` entry for a protocol-2 SDK that speaks metrics.
pub const CAP_METRICS: &str = "metrics";

/// `welcome.perf`: where rings live and the SDK's clock.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PerfWelcome {
    /// `Z:\dev\shm\quiver-<uid>\<session>` (see [`crate::session::to_windows_path`]).
    pub session_dir_win: String,
    pub clock: Clock,
}

/// The perf messages. `D` is the descriptor type in `metrics.defs`: quiverd parses
/// `PerfMessage<serde_json::Value>` and then each def on its own, so one bad def is rejected
/// with a reason instead of failing the whole message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PerfMessage<D = MetricDescriptor> {
    /// mod →: declare or update metrics (runtime declarations).
    Metrics { defs: Vec<D> },
    /// ←: indices for ring records and `m`/`mark`, by id; and what was rejected, why.
    MetricsOk {
        ids: BTreeMap<String, u16>,
        #[serde(default)]
        rejected: BTreeMap<String, String>,
    },
    /// mod →: gauges, states, text, counters, coalesced to ≤ 10/s.
    M {
        ts: i64,
        #[serde(deserialize_with = "index_map")]
        v: BTreeMap<u16, SampleValue>,
    },
    /// mod →: a low-rate event.
    Mark {
        index: u16,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dur: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// mod →: ask for a ring of `bytes` (≤ 1 MiB).
    RingReq { bytes: u64 },
    /// ←: the bridge created and sized the file; the SDK maps it.
    Ring { path_win: String, bytes: u64, nonce: String },
    /// ←: whether anyone consumes per-frame data.
    PerfCtl { rings: Switch, detail: Detail },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Switch {
    On,
    Off,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    Basic,
    Full,
}

/// `{ "1": value }` keys as metric indices. (Internally tagged enums buffer their content,
/// which leaves JSON object keys as strings that serde's u16 won't take directly.)
fn index_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<u16, SampleValue>, D::Error> {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct Key(u16);
    impl<'de> Deserialize<'de> for Key {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Key, D::Error> {
            struct V;
            impl serde::de::Visitor<'_> for V {
                type Value = Key;
                fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                    f.write_str("a metric index")
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Key, E> {
                    v.parse().map(Key).map_err(|_| E::invalid_value(serde::de::Unexpected::Str(v), &self))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Key, E> {
                    u16::try_from(v).map(Key).map_err(|_| E::invalid_value(serde::de::Unexpected::Unsigned(v), &self))
                }
            }
            d.deserialize_any(V)
        }
    }
    let m = BTreeMap::<Key, SampleValue>::deserialize(d)?;
    Ok(m.into_iter().map(|(k, v)| (k.0, v)).collect())
}

/// The nonce as it travels in `ring.nonce`: 32 lowercase hex digits.
pub fn nonce_to_hex(n: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(32);
    for b in n {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

pub fn nonce_from_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut n = [0u8; 16];
    for (i, b) in n.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn wire_shapes() {
        let m: PerfMessage = serde_json::from_str(r#"{"t": "m", "ts": 123, "v": {"1": 2.5, "3": "on"}, "extra": 1}"#).unwrap();
        let PerfMessage::M { ts, v } = &m else { panic!() };
        assert_eq!((*ts, v[&1].clone(), v[&3].clone()), (123, SampleValue::Number(2.5), SampleValue::Text("on".into())));
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["t"], "m");
        let ok = PerfMessage::<MetricDescriptor>::MetricsOk { ids: [("aco-dlss.frame".to_string(), 1)].into(), rejected: BTreeMap::new() };
        assert_eq!(serde_json::to_string(&ok).unwrap(), r#"{"t":"metrics_ok","ids":{"aco-dlss.frame":1},"rejected":{}}"#);
        let ctl: PerfMessage = serde_json::from_str(r#"{"t": "perf_ctl", "rings": "on", "detail": "basic"}"#).unwrap();
        assert_eq!(ctl, PerfMessage::PerfCtl { rings: Switch::On, detail: Detail::Basic });
        let mark: PerfMessage = serde_json::from_str(r#"{"t": "mark", "index": 9, "ts": 5, "label": "begin"}"#).unwrap();
        assert_eq!(mark, PerfMessage::Mark { index: 9, ts: 5, dur: None, label: Some("begin".into()) });
        // Defs parsed one by one: the bad one doesn't sink the message.
        let raw: PerfMessage<serde_json::Value> =
            serde_json::from_str(r#"{"t": "metrics", "defs": [{"id": "m.a", "kind": "gauge"}, {"id": "m.b", "kind": "bogus"}]}"#).unwrap();
        let PerfMessage::Metrics { defs } = raw else { panic!() };
        let parsed: Vec<_> = defs.into_iter().map(serde_json::from_value::<MetricDescriptor>).collect();
        assert!(parsed[0].is_ok() && parsed[1].is_err());
        // Unknown message types are an error the caller ignores.
        assert!(serde_json::from_str::<PerfMessage>(r#"{"t": "hello", "proto": 2}"#).is_err());
        let w = PerfWelcome { session_dir_win: r"Z:\dev\shm\quiver-1000\s-1".into(), clock: Clock::Qpc };
        assert_eq!(serde_json::to_value(&w).unwrap()["clock"], "qpc");
    }

    #[test]
    fn nonce_hex() {
        let n = [0, 1, 0xab, 0xff, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let h = nonce_to_hex(&n);
        assert_eq!(h, "0001abff0405060708090a0b0c0d0e0f");
        assert_eq!(nonce_from_hex(&h), Some(n));
        assert_eq!(nonce_from_hex("zz"), None);
        assert_eq!(nonce_from_hex(&"g".repeat(32)), None);
        assert_eq!(nonce_from_hex("é0001abff0405060708090a0b0c0d0e"), None);
    }
}
