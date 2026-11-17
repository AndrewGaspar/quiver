//! Value formatting (decision 3): quiverd turns every value into the string the HUD, the
//! CLI, agents and the app show, with a theme level from the descriptor's thresholds.

use quiver_api::perf::{Level, PerfValue};
use quiver_perf::descriptor::{Format, MetricDescriptor, MetricKind, Threshold, Unit};

/// A value before formatting.
#[derive(Clone, Debug, PartialEq)]
pub enum Raw {
    Num(f64),
    Text(String),
    /// Index into the descriptor's `values`.
    State(usize),
}

impl Raw {
    pub fn num(&self) -> Option<f64> {
        match self {
            Raw::Num(v) => Some(*v),
            _ => None,
        }
    }
}

pub const NO_VALUE: &str = "—";

/// Formats `raw` for `desc`. `None` (or a non-finite number) shows `—`, dimmed.
pub fn format(desc: &MetricDescriptor, raw: Option<&Raw>) -> PerfValue {
    let none = || PerfValue { text: NO_VALUE.into(), level: Level::Dim, ..Default::default() };
    match raw {
        None => none(),
        Some(Raw::Num(v)) if !v.is_finite() => none(),
        Some(Raw::Num(v)) => {
            let (text, unit) = number(*v, desc.unit, desc.format.as_ref());
            PerfValue { value: Some(*v), text, unit, level: level_for(*v, &desc.thresholds), stale: false }
        }
        Some(Raw::Text(s)) => PerfValue { text: if s.is_empty() { NO_VALUE.into() } else { s.clone() }, ..Default::default() },
        Some(Raw::State(i)) => match desc.values.get(*i) {
            Some(sv) => PerfValue {
                text: sv.label.clone().unwrap_or_else(|| sv.value.clone()),
                level: sv.level.unwrap_or(Level::Normal),
                ..Default::default()
            },
            None => none(),
        },
    }
}

/// A stale value: `—`, dimmed, flagged.
pub fn stale() -> PerfValue {
    PerfValue { text: NO_VALUE.into(), level: Level::Dim, stale: true, ..Default::default() }
}

/// The most severe level whose threshold `v` crosses (bad > warn > good > dim).
pub fn level_for(v: f64, thresholds: &[Threshold]) -> Level {
    let rank = |l: Level| match l {
        Level::Bad => 4,
        Level::Warn => 3,
        Level::Good => 2,
        Level::Dim => 1,
        _ => 0,
    };
    thresholds
        .iter()
        .filter(|t| t.above.is_some_and(|a| v > a) || t.below.is_some_and(|b| v < b))
        .map(|t| t.level)
        .max_by_key(|l| rank(*l))
        .unwrap_or(Level::Normal)
}

/// (text, unit) for a number in `unit`.
pub fn number(v: f64, unit: Unit, fmt: Option<&Format>) -> (String, String) {
    let fmt = fmt.copied().unwrap_or_default();
    let prec = |default: u8| fmt.precision.unwrap_or(default) as usize;
    let signed = |s: String| if fmt.sign && v > 0.0 { format!("+{s}") } else { s };
    let fixed = |x: f64, p: usize| signed(format!("{x:.p$}"));
    match unit {
        Unit::Fps => (fixed(v, prec(0)), String::new()),
        Unit::Ms => (fixed(v, prec(1)), "ms".into()),
        Unit::Us => (fixed(v, prec(0)), "µs".into()),
        Unit::Ns => (fixed(v, prec(0)), "ns".into()),
        Unit::S => (duration(v), String::new()),
        Unit::Percent => {
            let x = if fmt.percent { v * 100.0 } else { v };
            (fixed(x, prec(0)), "%".into())
        }
        Unit::Celsius => (fixed(v, prec(0)), "°".into()),
        Unit::Watt => (fixed(v, prec(if v.abs() < 10.0 { 1 } else { 0 })), "W".into()),
        Unit::MHz => (fixed(v, prec(0)), "MHz".into()),
        Unit::Hz => (fixed(v, prec(0)), "Hz".into()),
        Unit::Bytes => bytes(v, fmt.precision, ""),
        Unit::BytesPerSec => bytes(v, fmt.precision, "/s"),
        Unit::Ratio => (fixed(v, prec(1)), "×".into()),
        Unit::Px => (fixed(v, prec(0)), "px".into()),
        Unit::Count => (fixed(v, prec(0)), String::new()),
        Unit::None => {
            let p = fmt.precision.map(usize::from).unwrap_or(if v.fract() == 0.0 { 0 } else { 2 });
            let x = if fmt.percent { v * 100.0 } else { v };
            (fixed(x, p), if fmt.percent { "%".into() } else { String::new() })
        }
    }
}

/// 1024-based, labelled the way people read VRAM and RAM ("6.1 GB").
fn bytes(v: f64, precision: Option<u8>, suffix: &str) -> (String, String) {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut x = v;
    let mut i = 0;
    while x.abs() >= 1024.0 && i < UNITS.len() - 1 {
        x /= 1024.0;
        i += 1;
    }
    let p = precision.map(usize::from).unwrap_or(if i >= 3 { 1 } else { 0 });
    (format!("{x:.p$}"), format!("{}{suffix}", UNITS[i]))
}

/// Seconds as `m:ss` or `h:mm:ss`.
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// The stat a metric shows when none is asked for: `mean` for per-frame metrics and
/// gauges sampled fast, `last` for states, text and slow gauges.
pub fn default_stat(desc: &MetricDescriptor) -> quiver_api::perf::Stat {
    use quiver_api::perf::Stat;
    match desc.kind {
        MetricKind::Counter => Stat::Rate,
        MetricKind::Frame | MetricKind::Series => Stat::Mean,
        MetricKind::Gauge if desc.sample == Some(quiver_perf::descriptor::SampleRate::PerFrame) => Stat::Mean,
        _ => Stat::Last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_perf::descriptor::{StateValue, Threshold};

    fn d(unit: Unit) -> MetricDescriptor {
        let mut d = MetricDescriptor::new("x.y", MetricKind::Gauge);
        d.unit = unit;
        d
    }

    #[test]
    fn numbers_by_unit() {
        let f = |v, u| {
            let p = format(&d(u), Some(&Raw::Num(v)));
            p.display()
        };
        assert_eq!(f(158.4, Unit::Fps), "158");
        assert_eq!(f(12.66, Unit::Ms), "12.7 ms");
        assert_eq!(f(97.2, Unit::Percent), "97%");
        assert_eq!(f(71.0, Unit::Celsius), "71°");
        assert_eq!(f(92.4, Unit::Watt), "92 W");
        assert_eq!(f(4.25, Unit::Watt), "4.2 W");
        assert_eq!(f(6.1 * 1024.0 * 1024.0 * 1024.0, Unit::Bytes), "6.1 GB");
        assert_eq!(f(512.0 * 1024.0 * 1024.0, Unit::Bytes), "512 MB");
        assert_eq!(f(2.0, Unit::Ratio), "2.0×");
        assert_eq!(f(3725.0, Unit::S), "1:02:05");
        assert_eq!(f(65.0, Unit::S), "1:05");
        assert_eq!(f(165.0, Unit::Hz), "165 Hz");
        assert_eq!(f(3.0, Unit::None), "3");
        assert_eq!(f(f64::NAN, Unit::Ms), "—");
        let mut pct = d(Unit::Percent);
        pct.format = Some(Format { percent: true, ..Default::default() });
        assert_eq!(format(&pct, Some(&Raw::Num(0.42))).display(), "42%");
        let mut signed = d(Unit::Ms);
        signed.format = Some(Format { sign: true, precision: Some(2), ..Default::default() });
        assert_eq!(format(&signed, Some(&Raw::Num(1.5))).display(), "+1.50 ms");
    }

    #[test]
    fn levels_from_thresholds() {
        let t = [
            Threshold { above: Some(30.0), below: None, level: Level::Warn },
            Threshold { above: Some(50.0), below: None, level: Level::Bad },
            Threshold { above: None, below: Some(10.0), level: Level::Good },
        ];
        assert_eq!(level_for(21.0, &t), Level::Normal);
        assert_eq!(level_for(35.0, &t), Level::Warn);
        assert_eq!(level_for(60.0, &t), Level::Bad);
        assert_eq!(level_for(5.0, &t), Level::Good);
    }

    #[test]
    fn states_text_and_missing() {
        let mut s = MetricDescriptor::new("m.fg", MetricKind::State);
        s.values = vec![
            StateValue { value: "off".into(), label: Some("off".into()), level: Some(Level::Dim) },
            StateValue { value: "on".into(), label: None, level: None },
        ];
        let off = format(&s, Some(&Raw::State(0)));
        assert_eq!((off.text.as_str(), off.level), ("off", Level::Dim));
        assert_eq!(format(&s, Some(&Raw::State(1))).text, "on");
        assert_eq!(format(&s, Some(&Raw::State(9))).text, NO_VALUE);
        assert_eq!(format(&s, None).level, Level::Dim);
        let t = MetricDescriptor::new("m.t", MetricKind::Text);
        assert_eq!(format(&t, Some(&Raw::Text("DLSS K".into()))).text, "DLSS K");
        assert!(stale().stale);
    }
}
