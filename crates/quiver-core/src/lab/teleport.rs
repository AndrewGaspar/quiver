//! `quiver lab teleport` and `turn`: named locations (`locations.txt`) and turning the game's
//! camera to a heading with mouse-look, in a closed loop on the mod's `where` (design/LAB.md §6).
//!
//! A mod can move the player from inside the game but often can't turn the camera: under Wine,
//! `SendInput` from inside the game never reaches a raw-input mouse-look (AC Origins'), while
//! XTEST motion from the host does. So a mod answers `teleport x y z yaw pitch` by moving the
//! player and returning `"want":{"yaw":…,"pitch":…}`, and the lab turns the camera: read it
//! with `where`, move the mouse, read again, until it is within a tolerance or a bounded
//! number of moves is spent. The mouse's gain (degrees per pixel, either sign) is measured
//! from the moves themselves, so sensitivity, inverted axes and the game's yaw convention
//! don't matter; a learned gain is kept per instance as the next turn's first guess.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A location's arguments for the mod's `teleport` verb: the row of `locations.txt`
/// (`name x y z [yaw [pitch]] [# comment]`) named `name`, without the name or the comment.
/// Blank and comment-only lines are skipped.
pub fn location(table: &str, name: &str) -> Option<Vec<String>> {
    table.lines().find_map(|l| {
        let l = l.split_once('#').map_or(l, |(a, _)| a);
        let mut w = l.split_whitespace();
        (w.next() == Some(name)).then(|| w.map(str::to_owned).collect())
    })
}

/// The names in `locations.txt`.
pub fn location_names(table: &str) -> Vec<String> {
    table.lines().filter_map(|l| l.split_once('#').map_or(l, |(a, _)| a).split_whitespace().next().map(str::to_owned)).collect()
}

/// The camera as the mod's `where` reports it: `camera.yaw`/`camera.pitch` (degrees), else
/// top-level `yaw`/`pitch`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Camera {
    pub yaw: f64,
    pub pitch: Option<f64>,
}

pub fn camera_of(where_result: &Value) -> Option<Camera> {
    let from = |v: &Value| v.get("yaw").and_then(Value::as_f64).map(|yaw| Camera { yaw, pitch: v.get("pitch").and_then(Value::as_f64) });
    where_result.get("camera").and_then(from).or_else(|| from(where_result))
}

/// The heading a mod's `teleport` answer asks the lab to turn to: `want.yaw` and `want.pitch`.
pub fn want_of(result: &Value) -> Option<(f64, Option<f64>)> {
    let w = result.get("want")?;
    Some((w.get("yaw")?.as_f64()?, w.get("pitch").and_then(Value::as_f64)))
}

/// Degrees the camera turns per pixel of relative mouse motion, per axis (signed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Gain {
    pub yaw: Option<f64>,
    pub pitch: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct TurnOpts {
    /// Degrees, on each axis.
    pub tolerance: f64,
    /// Mouse moves at most (the probe included).
    pub max_moves: u32,
    /// A first guess (a gain learned by an earlier turn).
    pub gain: Gain,
}

impl Default for TurnOpts {
    fn default() -> Self {
        TurnOpts { tolerance: 0.5, max_moves: 10, gain: Gain::default() }
    }
}

/// How a turn went.
#[derive(Debug, Clone, Serialize)]
pub struct Turned {
    pub camera: Camera,
    pub moves: u32,
    pub converged: bool,
    /// Degrees still to go (yaw, pitch).
    pub error: (f64, f64),
    pub gain: Gain,
    /// Why it stopped short, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What the turn drives: the camera readout and the mouse.
pub trait Rig {
    /// The camera now (the mod's `where`).
    fn read(&mut self) -> Result<Camera>;
    /// Relative mouse motion, in pixels.
    fn mouse(&mut self, dx: i32, dy: i32) -> Result<()>;
    /// Let the game catch up (camera smoothing): real time, in a game.
    fn wait_ms(&mut self, ms: u64);
}

/// The first move on an axis with no gain yet, in pixels.
const PROBE_PX: i32 = 100;
/// The most one move turns, in pixels (several XTEST steps; the game clamps huge deltas).
const MAX_MOVE_PX: i32 = 8000;
/// Changes smaller than this (degrees) are noise.
const STILL: f64 = 0.02;

/// `a - b` on a circle of degrees, in [-180, 180).
pub fn angle_diff(a: f64, b: f64) -> f64 {
    (a - b + 180.0).rem_euclid(360.0) - 180.0
}

/// Read until two readings 100 ms apart agree (the camera's smoothing has caught up), at
/// most 2.5 s.
fn settled(rig: &mut impl Rig) -> Result<Camera> {
    let mut last = rig.read()?;
    for _ in 0..25 {
        rig.wait_ms(100);
        let now = rig.read()?;
        let still = angle_diff(now.yaw, last.yaw).abs() < STILL && (now.pitch.unwrap_or(0.0) - last.pitch.unwrap_or(0.0)).abs() < STILL;
        last = now;
        if still {
            break;
        }
    }
    Ok(last)
}

/// One axis of the loop: its gain and how often a move did nothing.
struct Axis {
    gain: Option<f64>,
    stalls: u32,
    /// Gave up: the game doesn't turn this way any further (pitch at its limit).
    stuck: bool,
}

impl Axis {
    /// Pixels for an error of `e` degrees (0 within tolerance).
    fn pixels(&self, e: f64, tol: f64) -> i32 {
        if e.abs() <= tol || self.stuck {
            return 0;
        }
        let px = match self.gain {
            Some(g) => (e / g).round(),
            // No gain yet: a probe, made larger after one that did nothing.
            None => (PROBE_PX * 4i32.pow(self.stalls.min(2))) as f64,
        };
        let px = px.clamp(-(MAX_MOVE_PX as f64), MAX_MOVE_PX as f64) as i32;
        if px == 0 { if e * self.gain.unwrap_or(1.0) > 0.0 { 1 } else { -1 } } else { px }
    }

    /// After moving `px` pixels the camera turned `seen` degrees.
    fn learn(&mut self, px: i32, seen: f64) {
        if px == 0 {
            return;
        }
        if seen.abs() < STILL {
            self.stalls += 1;
            // A known gain that stops working twice: the game won't turn further this way.
            if self.gain.is_some() && self.stalls >= 2 {
                self.stuck = true;
            }
            return;
        }
        self.stalls = 0;
        if px.abs() >= 4 {
            self.gain = Some(seen / px as f64);
        }
    }
}

/// Turn the camera to `yaw` (and `pitch`), within `o.tolerance`, in at most `o.max_moves`
/// mouse moves.
pub fn turn(rig: &mut impl Rig, yaw: f64, pitch: Option<f64>, o: &TurnOpts) -> Result<Turned> {
    let mut x = Axis { gain: o.gain.yaw.filter(|g| g.is_finite() && *g != 0.0), stalls: 0, stuck: false };
    let mut y = Axis { gain: o.gain.pitch.filter(|g| g.is_finite() && *g != 0.0), stalls: 0, stuck: false };
    let mut cam = settled(rig)?;
    if pitch.is_some() && cam.pitch.is_none() {
        bail!("the mod's `where` reports no camera pitch (camera.pitch), so pitch can't be turned to");
    }
    let errors = |c: &Camera| (angle_diff(yaw, c.yaw), pitch.zip(c.pitch).map_or(0.0, |(w, p)| w - p));
    let mut moves = 0;
    let mut note = None;
    loop {
        let (ey, ep) = errors(&cam);
        if ey.abs() <= o.tolerance && ep.abs() <= o.tolerance {
            break;
        }
        if moves >= o.max_moves {
            note = Some(format!("not within {}° after {moves} moves", o.tolerance));
            break;
        }
        let (dx, dy) = (x.pixels(ey, o.tolerance), y.pixels(ep, o.tolerance));
        if dx == 0 && dy == 0 {
            note = Some("the camera stopped turning (pitch at the game's limit?)".into());
            break;
        }
        rig.mouse(dx, dy)?;
        moves += 1;
        let now = settled(rig)?;
        x.learn(dx, angle_diff(now.yaw, cam.yaw));
        y.learn(dy, now.pitch.unwrap_or(0.0) - cam.pitch.unwrap_or(0.0));
        cam = now;
        // Nothing turned after the largest probe on an axis that has no gain: the mouse
        // doesn't reach the camera.
        for (a, name) in [(&x, "yaw"), (&y, "pitch")] {
            if a.gain.is_none() && a.stalls > 2 {
                bail!(
                    "the camera's {name} didn't change with mouse motion ({moves} moves): is the game in the world, its window focused, and not paused (quiver lab clock)?"
                );
            }
        }
    }
    let (ey, ep) = errors(&cam);
    let converged = ey.abs() <= o.tolerance && ep.abs() <= o.tolerance;
    Ok(Turned { camera: cam, moves, converged, error: (ey, ep), gain: Gain { yaw: x.gain, pitch: y.gain }, note: if converged { None } else { note } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_lose_their_comments() {
        let t = "# Origins\n\ngiza-sphinx -749.7 3574.2 207.8 90 -5   # the Sphinx, from the east\nsiwa 1 2 3#no space\nalex 4 5 6\n";
        assert_eq!(location(t, "giza-sphinx").unwrap(), ["-749.7", "3574.2", "207.8", "90", "-5"]);
        assert_eq!(location(t, "siwa").unwrap(), ["1", "2", "3"]);
        assert_eq!(location(t, "alex").unwrap(), ["4", "5", "6"]);
        assert!(location(t, "#").is_none() && location(t, "Origins").is_none() && location(t, "nope").is_none());
        assert_eq!(location_names(t), ["giza-sphinx", "siwa", "alex"]);
    }

    #[test]
    fn cameras_and_wants_parse() {
        let w: Value = serde_json::json!({"pos":[1,2,3],"yaw":12.0,"camera":{"pos":[1,2,4],"yaw":91.5,"pitch":-4.0}});
        assert_eq!(camera_of(&w), Some(Camera { yaw: 91.5, pitch: Some(-4.0) }));
        assert_eq!(camera_of(&serde_json::json!({"yaw":10.0})), Some(Camera { yaw: 10.0, pitch: None }));
        assert_eq!(camera_of(&serde_json::json!({"pos":[0,0,0]})), None);
        let t: Value = serde_json::json!({"status":"ok","want":{"yaw":90.0,"pitch":-5.0}});
        assert_eq!(want_of(&t), Some((90.0, Some(-5.0))));
        assert_eq!(want_of(&serde_json::json!({"status":"ok","want":{"yaw":90.0}})), Some((90.0, None)));
        assert_eq!(want_of(&serde_json::json!({"status":"ok"})), None);
    }

    #[test]
    fn angles_wrap() {
        assert_eq!(angle_diff(10.0, 350.0), 20.0);
        assert_eq!(angle_diff(350.0, 10.0), -20.0);
        assert_eq!(angle_diff(-179.0, 179.0), 2.0);
        assert_eq!(angle_diff(90.0, 90.0), 0.0);
    }

    /// A game's camera: mouse-look with a gain per axis (Origins': yaw -0.117 °/px, so moving
    /// right turns clockwise in its counter-clockwise yaw; pitch -0.077 °/px), smoothing that
    /// takes a few reads to catch up, a pitch limit, and optionally a mouse that does nothing.
    struct Game {
        yaw: f64,
        pitch: f64,
        target: (f64, f64),
        gain: (f64, f64),
        limit: f64,
        smooth: f64,
        dead: bool,
        moves: Vec<(i32, i32)>,
        reads: u32,
    }

    impl Game {
        fn origins(yaw: f64, pitch: f64) -> Game {
            Game { yaw, pitch, target: (yaw, pitch), gain: (-0.117, -0.077), limit: 80.0, smooth: 0.5, dead: false, moves: vec![], reads: 0 }
        }
    }

    impl Rig for Game {
        fn read(&mut self) -> Result<Camera> {
            self.reads += 1;
            Ok(Camera { yaw: (self.yaw + 180.0).rem_euclid(360.0) - 180.0, pitch: Some(self.pitch) })
        }
        fn mouse(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.moves.push((dx, dy));
            if !self.dead {
                self.target.0 += dx as f64 * self.gain.0;
                self.target.1 = (self.target.1 + dy as f64 * self.gain.1).clamp(-self.limit, self.limit);
            }
            Ok(())
        }
        fn wait_ms(&mut self, _ms: u64) {
            // Smoothing: each 100 ms closes `smooth` of the gap.
            self.yaw += (self.target.0 - self.yaw) * self.smooth;
            self.pitch += (self.target.1 - self.pitch) * self.smooth;
        }
    }

    #[test]
    fn turns_to_a_heading_without_knowing_the_gain() {
        let mut g = Game::origins(12.0, 0.0);
        let r = turn(&mut g, 90.0, Some(-5.0), &TurnOpts::default()).unwrap();
        assert!(r.converged, "{r:?}");
        assert!(angle_diff(90.0, r.camera.yaw).abs() <= 0.5 && (r.camera.pitch.unwrap() + 5.0).abs() <= 0.5, "{r:?}");
        assert!(r.moves <= 5, "{} moves: {:?}", r.moves, g.moves);
        // It learned Origins' gains.
        assert!((r.gain.yaw.unwrap() + 0.117).abs() < 0.01 && (r.gain.pitch.unwrap() + 0.077).abs() < 0.01, "{:?}", r.gain);
        // With them, the next turn goes straight there.
        let mut g = Game::origins(-170.0, 10.0);
        let r = turn(&mut g, 170.0, Some(0.0), &TurnOpts { gain: r.gain, ..Default::default() }).unwrap();
        assert!(r.converged && r.moves <= 2, "{r:?} {:?}", g.moves);
        // The short way round: -170 to 170 is 20 degrees, not 340.
        assert!(g.moves[0].0.abs() < 400, "{:?}", g.moves);
    }

    #[test]
    fn a_wrong_first_guess_and_other_conventions_still_converge() {
        // A game whose yaw turns the other way, with a stale gain of the wrong sign and size.
        let mut g = Game::origins(0.0, 0.0);
        g.gain = (0.05, 0.03);
        let r = turn(&mut g, -120.0, Some(20.0), &TurnOpts { gain: Gain { yaw: Some(-0.117), pitch: Some(-0.077) }, ..Default::default() }).unwrap();
        assert!(r.converged, "{r:?} {:?}", g.moves);
        assert!(r.gain.yaw.unwrap() > 0.0 && r.gain.pitch.unwrap() > 0.0);
    }

    #[test]
    fn yaw_only_and_already_there() {
        let mut g = Game::origins(45.2, 3.0);
        let r = turn(&mut g, 45.0, None, &TurnOpts::default()).unwrap();
        assert!(r.converged && r.moves == 0 && g.moves.is_empty());
        let r = turn(&mut g, -45.0, None, &TurnOpts::default()).unwrap();
        assert!(r.converged && g.moves.iter().all(|m| m.1 == 0), "pitch untouched: {:?}", g.moves);
    }

    #[test]
    fn bounded_and_honest_when_it_cant_get_there() {
        // Pitch past the game's limit: yaw gets there, pitch stops at the limit, not converged.
        let mut g = Game::origins(0.0, 0.0);
        let r = turn(&mut g, 30.0, Some(-89.0), &TurnOpts::default()).unwrap();
        assert!(!r.converged && angle_diff(30.0, r.camera.yaw).abs() <= 0.5, "{r:?}");
        assert!((r.camera.pitch.unwrap() + 80.0).abs() < 0.5 && r.note.is_some(), "{r:?}");
        assert!(r.moves <= 10);
        // Never more than max_moves.
        let mut g = Game::origins(0.0, 0.0);
        g.smooth = 0.05; // so slow the readings never settle in time: every move overshoots
        let r = turn(&mut g, 179.0, None, &TurnOpts { max_moves: 3, ..Default::default() }).unwrap();
        assert!(g.moves.len() <= 3, "{:?}", g.moves);
        assert!(r.converged || r.note.as_deref().is_some_and(|n| n.contains("3 moves")), "{r:?}");
        // A mouse that never reaches the camera: an error, after a few probes.
        let mut g = Game::origins(0.0, 0.0);
        g.dead = true;
        let e = turn(&mut g, 90.0, None, &TurnOpts::default()).unwrap_err().to_string();
        assert!(e.contains("didn't change with mouse motion"), "{e}");
        assert!(g.moves.len() <= 4, "{:?}", g.moves);
        // Pitch asked for but not reported.
        struct NoPitch;
        impl Rig for NoPitch {
            fn read(&mut self) -> Result<Camera> {
                Ok(Camera { yaw: 0.0, pitch: None })
            }
            fn mouse(&mut self, _: i32, _: i32) -> Result<()> {
                Ok(())
            }
            fn wait_ms(&mut self, _: u64) {}
        }
        assert!(turn(&mut NoPitch, 10.0, Some(0.0), &TurnOpts::default()).is_err());
    }
}
