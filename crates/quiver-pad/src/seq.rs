//! Pad scripts: `quiver lab pad N seq "a 100ms; wait 200ms; lx=1 500ms; +rt=1; start; neutral"`.
//!
//! Steps are separated by `;` or new lines (`#` starts a comment). A step's words:
//!
//! - `a`, `lb`, `up` …: the button is down **for this step** (its duration), then up again;
//! - `lx=0.5`, `rt=1`, `lstick=0,1`, `rstick=-1,0`: the axis is there for this step, then back
//!   where it was;
//! - `+a`, `+lx=0.5`: down / set, and it **stays** (until `-a`, another value, or `neutral`);
//! - `-a`: up; `neutral`: every button up and every axis centred (stays);
//! - a duration: `120ms`, `1.5s`, or `12f` (frames: needs the virtual clock); `wait` is a
//!   step that only waits. A step with momentary words and no duration lasts the default hold.
//!
//! Sticks are -1..1 with Y up (XInput's way), triggers 0..1. The grips of a Steam Controller
//! (`l4 l5 r4 r5`) are accepted when a grip map says which buttons they are (a hyprpad user's
//! `pad_map`, [`crate::model::parse_grip_map`]).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{Axis, Button, Change, PadState};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Dur {
    Ms(f64),
    Frames(u64),
}

impl Dur {
    pub fn parse(s: &str) -> Option<Dur> {
        let s = s.trim().to_ascii_lowercase();
        if let Some(n) = s.strip_suffix("ms") {
            return n.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0).map(Dur::Ms);
        }
        if let Some(n) = s.strip_suffix('s') {
            return n.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0).map(|v| Dur::Ms(v * 1000.0));
        }
        if let Some(n) = s.strip_suffix('f') {
            return n.parse::<u64>().ok().map(Dur::Frames);
        }
        None
    }

    /// Frames at `rate` Hz: the nearest whole number, at least one for a non-zero duration.
    pub fn frames(&self, rate: f64) -> u64 {
        match *self {
            Dur::Frames(f) => f,
            Dur::Ms(ms) if ms <= 0.0 => 0,
            Dur::Ms(ms) => ((ms * rate / 1000.0).round() as u64).max(1),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Dur::Ms(ms) => format!("{ms:.0} ms"),
            Dur::Frames(f) => format!("{f} frame(s)"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Apply(Vec<Change>),
    Wait(Dur),
}

fn button(word: &str, grips: &[(String, Button)]) -> Result<Button> {
    let w = word.to_ascii_lowercase();
    if let Some((_, b)) = grips.iter().find(|(g, _)| *g == w) {
        return Ok(*b);
    }
    if ["l4", "l5", "r4", "r5"].contains(&w.as_str()) {
        bail!("'{word}' is a Steam Controller grip, not an Xbox button: give a grip map (--grips r4=a,r5=b,l4=up,l5=down, or the instance's pad.grips)");
    }
    Button::parse(word).with_context(|| format!("'{word}' isn't a pad button (a b x y lb rb back start guide ls rs up down left right)"))
}

fn value(s: &str) -> Result<f32> {
    let v: f32 = s.trim().parse().with_context(|| format!("'{s}' isn't a number"))?;
    if !v.is_finite() {
        bail!("'{s}' isn't a finite number");
    }
    Ok(v)
}

/// `name=v` or `lstick=x,y` as changes.
fn assignment(lhs: &str, rhs: &str) -> Result<Vec<Change>> {
    let l = lhs.to_ascii_lowercase();
    let stick = match l.as_str() {
        "lstick" | "left_stick" | "leftstick" => Some((Axis::Lx, Axis::Ly)),
        "rstick" | "right_stick" | "rightstick" => Some((Axis::Rx, Axis::Ry)),
        _ => None,
    };
    if let Some((ax, ay)) = stick {
        let (x, y) = rhs.split_once(',').with_context(|| format!("{lhs}=X,Y"))?;
        return Ok(vec![Change::Axis { axis: ax, value: value(x)? }, Change::Axis { axis: ay, value: value(y)? }]);
    }
    let axis = Axis::parse(&l).with_context(|| format!("'{lhs}' isn't an axis (lx ly rx ry lt rt lstick rstick)"))?;
    Ok(vec![Change::Axis { axis, value: value(rhs)? }])
}

/// Compile a script against the pad's current state: each step's momentary words become a
/// change, a wait and the change back.
pub fn compile(script: &str, start: &PadState, default_hold: Dur, grips: &[(String, Button)]) -> Result<Vec<Action>> {
    let mut out = Vec::new();
    let mut state = *start;
    let text: String = script.lines().map(|l| l.split('#').next().unwrap_or("")).collect::<Vec<_>>().join(";");
    for (n, step) in text.split(';').map(str::trim).enumerate().filter(|(_, s)| !s.is_empty()) {
        let ctx = || format!("step {} ('{step}')", n + 1);
        let mut lasting: Vec<Change> = Vec::new();
        let mut momentary: Vec<Change> = Vec::new();
        let mut dur = None;
        for w in step.split_whitespace() {
            if let Some(d) = Dur::parse(w) {
                if dur.replace(d).is_some() {
                    bail!("{}: two durations", ctx());
                }
                continue;
            }
            match w.to_ascii_lowercase().as_str() {
                "wait" => continue,
                "neutral" | "release_all" => {
                    lasting.push(Change::Neutral);
                    continue;
                }
                _ => {}
            }
            let (sign, rest) = match w.as_bytes()[0] {
                b'+' => (Some(true), &w[1..]),
                b'-' if !w[1..].starts_with(|c: char| c.is_ascii_digit() || c == '.') => (Some(false), &w[1..]),
                _ => (None, w),
            };
            let changes = match rest.split_once('=') {
                Some((l, r)) => {
                    if sign == Some(false) {
                        bail!("{}: '-{rest}': use +{l}=0 to centre an axis", ctx());
                    }
                    assignment(l, r).with_context(ctx)?
                }
                None => {
                    let b = button(rest, grips).with_context(ctx)?;
                    vec![if sign == Some(false) { Change::Release { button: b } } else { Change::Press { button: b } }]
                }
            };
            if sign.is_some() { lasting.extend(changes) } else { momentary.extend(changes) }
        }
        if !lasting.is_empty() {
            for c in &lasting {
                state.apply(c);
            }
            out.push(Action::Apply(lasting));
        }
        if momentary.is_empty() {
            if let Some(d) = dur {
                out.push(Action::Wait(d));
            }
            continue;
        }
        let before = state;
        for c in &momentary {
            state.apply(c);
        }
        // Back to how it was: buttons that were up go up, axes go back.
        let mut undo = Vec::new();
        for c in &momentary {
            match c {
                Change::Press { button } if !before.pressed(*button) => undo.push(Change::Release { button: *button }),
                Change::Axis { axis, .. } => undo.push(Change::Axis { axis: *axis, value: before.axis(*axis) }),
                _ => {}
            }
        }
        out.push(Action::Apply(momentary));
        out.push(Action::Wait(dur.unwrap_or(default_hold)));
        for c in &undo {
            state.apply(c);
        }
        if !undo.is_empty() {
            out.push(Action::Apply(undo));
        }
    }
    Ok(out)
}

/// The state after running `actions` from `start`.
pub fn end_state(start: &PadState, actions: &[Action]) -> PadState {
    let mut s = *start;
    for a in actions {
        if let Action::Apply(cs) = a {
            for c in cs {
                s.apply(c);
            }
        }
    }
    s
}

/// The total time of a script (ms, frames).
pub fn total(actions: &[Action], rate: Option<f64>) -> (f64, u64) {
    let mut ms = 0.0;
    let mut frames = 0;
    for a in actions {
        if let Action::Wait(d) = a {
            match (d, rate) {
                (Dur::Ms(m), _) => {
                    ms += m;
                    frames += rate.map(|r| d.frames(r)).unwrap_or(0);
                }
                (Dur::Frames(f), Some(r)) => {
                    frames += f;
                    ms += *f as f64 * 1000.0 / r;
                }
                (Dur::Frames(f), None) => frames += f,
            }
        }
    }
    (ms, frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(b: Button) -> Change {
        Change::Press { button: b }
    }
    fn release(b: Button) -> Change {
        Change::Release { button: b }
    }
    fn axis(a: Axis, v: f32) -> Change {
        Change::Axis { axis: a, value: v }
    }

    #[test]
    fn durations() {
        assert_eq!(Dur::parse("120ms"), Some(Dur::Ms(120.0)));
        assert_eq!(Dur::parse("1.5s"), Some(Dur::Ms(1500.0)));
        assert_eq!(Dur::parse("12f"), Some(Dur::Frames(12)));
        assert_eq!(Dur::parse("a"), None);
        assert_eq!(Dur::parse("-1s"), None);
        // 62.5 Hz: 16 ms a frame.
        assert_eq!(Dur::Ms(100.0).frames(62.5), 6);
        assert_eq!(Dur::Ms(1.0).frames(62.5), 1, "never zero frames for a real duration");
        assert_eq!(Dur::Ms(0.0).frames(62.5), 0);
        assert_eq!(Dur::Frames(7).frames(62.5), 7);
    }

    #[test]
    fn momentary_and_lasting() {
        let s = PadState::default();
        let a = compile("a 100ms; wait 200ms; lx=1 500ms; +rt=1 lt=0.5 300ms; +b; up; -b; neutral", &s, Dur::Ms(120.0), &[]).unwrap();
        assert_eq!(
            a,
            vec![
                Action::Apply(vec![press(Button::A)]),
                Action::Wait(Dur::Ms(100.0)),
                Action::Apply(vec![release(Button::A)]),
                Action::Wait(Dur::Ms(200.0)),
                Action::Apply(vec![axis(Axis::Lx, 1.0)]),
                Action::Wait(Dur::Ms(500.0)),
                Action::Apply(vec![axis(Axis::Lx, 0.0)]),
                Action::Apply(vec![axis(Axis::Rt, 1.0)]),
                Action::Apply(vec![axis(Axis::Lt, 0.5)]),
                Action::Wait(Dur::Ms(300.0)),
                Action::Apply(vec![axis(Axis::Lt, 0.0)]),
                Action::Apply(vec![press(Button::B)]),
                Action::Apply(vec![press(Button::Up)]),
                Action::Wait(Dur::Ms(120.0)),
                Action::Apply(vec![release(Button::Up)]),
                Action::Apply(vec![release(Button::B)]),
                Action::Apply(vec![Change::Neutral]),
            ]
        );
        assert_eq!(end_state(&s, &a), PadState::default());
        assert_eq!(total(&a, Some(62.5)), (1220.0, 6 + 13 + 31 + 19 + 8));
    }

    #[test]
    fn momentary_on_a_held_button_keeps_it_down() {
        let mut s = PadState::default();
        s.apply(&press(Button::A));
        s.apply(&axis(Axis::Lx, 0.25));
        let a = compile("a lx=1 2f", &s, Dur::Ms(120.0), &[]).unwrap();
        assert_eq!(a, vec![Action::Apply(vec![press(Button::A), axis(Axis::Lx, 1.0)]), Action::Wait(Dur::Frames(2)), Action::Apply(vec![axis(Axis::Lx, 0.25)])]);
    }

    #[test]
    fn sticks_comments_and_errors() {
        let a = compile("lstick=0,1 50ms # walk forward\nrstick=-1,-0.5", &PadState::default(), Dur::Ms(120.0), &[]).unwrap();
        assert_eq!(a[0], Action::Apply(vec![axis(Axis::Lx, 0.0), axis(Axis::Ly, 1.0)]));
        assert_eq!(a[3], Action::Apply(vec![axis(Axis::Rx, -1.0), axis(Axis::Ry, -0.5)]));
        assert!(compile("q", &PadState::default(), Dur::Ms(1.0), &[]).unwrap_err().to_string().contains("step 1"));
        assert!(format!("{:#}", compile("a 1s 2s", &PadState::default(), Dur::Ms(1.0), &[]).unwrap_err()).contains("two durations"));
        assert!(format!("{:#}", compile("r4", &PadState::default(), Dur::Ms(1.0), &[]).unwrap_err()).contains("grip"));
        let g = crate::model::parse_grip_map("r4=a").unwrap();
        assert_eq!(compile("r4 1f", &PadState::default(), Dur::Ms(1.0), &g).unwrap()[0], Action::Apply(vec![press(Button::A)]));
        assert!(compile("-lx=1", &PadState::default(), Dur::Ms(1.0), &[]).is_err());
        // A negative value isn't a release.
        assert_eq!(compile("lx=-1 1f", &PadState::default(), Dur::Ms(1.0), &[]).unwrap()[0], Action::Apply(vec![axis(Axis::Lx, -1.0)]));
    }
}
