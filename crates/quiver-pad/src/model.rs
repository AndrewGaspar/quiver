//! The pad's controls and state, and the evdev events a change of state becomes.
//!
//! The device is laid out as Linux's `xpad` driver lays out a wired Xbox 360 controller
//! (045e:028e), so SDL's mapping database, Wine's winebus and native games treat it as one:
//! eleven buttons, both sticks on ABS_X/Y and ABS_RX/RY (±32767), the triggers on ABS_Z and
//! ABS_RZ (0-255), the d-pad as hat 0.
//!
//! Values in the CLI and in [`PadState`] are in XInput's terms, which is how games and agents
//! think: stick Y is **positive up**, sticks are -1..1, triggers 0..1. evdev's Y is positive
//! down, so Y is flipped the way xpad flips it (`!y`).

use serde::{Deserialize, Serialize};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_FF: u16 = 0x15;
pub const SYN_REPORT: u16 = 0;
pub const FF_RUMBLE: u16 = 0x50;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

/// A wired Xbox 360 controller, as xpad reports it.
pub const VENDOR: u16 = 0x045e;
pub const PRODUCT: u16 = 0x028e;
pub const VERSION: u16 = 0x0114;
pub const BUS_USB: u16 = 0x03;
pub const IDENTITY: &str = "Xbox 360 Controller (045e:028e)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    A,
    B,
    X,
    Y,
    Lb,
    Rb,
    Back,
    Start,
    Guide,
    Ls,
    Rs,
    Up,
    Down,
    Left,
    Right,
}

impl Button {
    pub const ALL: [Button; 15] = [
        Button::A,
        Button::B,
        Button::X,
        Button::Y,
        Button::Lb,
        Button::Rb,
        Button::Back,
        Button::Start,
        Button::Guide,
        Button::Ls,
        Button::Rs,
        Button::Up,
        Button::Down,
        Button::Left,
        Button::Right,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Button::A => "a",
            Button::B => "b",
            Button::X => "x",
            Button::Y => "y",
            Button::Lb => "lb",
            Button::Rb => "rb",
            Button::Back => "back",
            Button::Start => "start",
            Button::Guide => "guide",
            Button::Ls => "ls",
            Button::Rs => "rs",
            Button::Up => "up",
            Button::Down => "down",
            Button::Left => "left",
            Button::Right => "right",
        }
    }

    /// Names and their usual aliases (XInput's, SDL's, PlayStation's shapes).
    pub fn parse(s: &str) -> Option<Button> {
        let s = s.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Some(match s.as_str() {
            "a" | "cross" | "south" => Button::A,
            "b" | "circle" | "east" => Button::B,
            "x" | "square" | "west" => Button::X,
            "y" | "triangle" | "north" => Button::Y,
            "lb" | "l1" | "leftshoulder" | "left_shoulder" => Button::Lb,
            "rb" | "r1" | "rightshoulder" | "right_shoulder" => Button::Rb,
            "back" | "select" | "view" | "share" => Button::Back,
            "start" | "menu" | "options" => Button::Start,
            "guide" | "home" | "xbox" | "ps" => Button::Guide,
            "ls" | "l3" | "leftstick" | "left_stick" | "lsb" => Button::Ls,
            "rs" | "r3" | "rightstick" | "right_stick" | "rsb" => Button::Rs,
            "up" | "dpad_up" | "dpup" | "du" => Button::Up,
            "down" | "dpad_down" | "dpdown" | "dd" => Button::Down,
            "left" | "dpad_left" | "dpleft" | "dl" => Button::Left,
            "right" | "dpad_right" | "dpright" | "dr" => Button::Right,
            _ => return None,
        })
    }

    fn bit(self) -> u32 {
        1 << (self as u32)
    }

    /// The evdev key code, for the eleven real buttons (the d-pad is a hat).
    pub fn key(self) -> Option<u16> {
        Some(match self {
            Button::A => 0x130,     // BTN_A / BTN_SOUTH
            Button::B => 0x131,     // BTN_B / BTN_EAST
            Button::X => 0x133,     // BTN_X / BTN_NORTH (xpad's X)
            Button::Y => 0x134,     // BTN_Y / BTN_WEST (xpad's Y)
            Button::Lb => 0x136,    // BTN_TL
            Button::Rb => 0x137,    // BTN_TR
            Button::Back => 0x13a,  // BTN_SELECT
            Button::Start => 0x13b, // BTN_START
            Button::Guide => 0x13c, // BTN_MODE
            Button::Ls => 0x13d,    // BTN_THUMBL
            Button::Rs => 0x13e,    // BTN_THUMBR
            _ => return None,
        })
    }

    /// XInput's `wButtons` bit for this button (what the probe reads back).
    pub fn xinput(self) -> u16 {
        match self {
            Button::Up => 0x0001,
            Button::Down => 0x0002,
            Button::Left => 0x0004,
            Button::Right => 0x0008,
            Button::Start => 0x0010,
            Button::Back => 0x0020,
            Button::Ls => 0x0040,
            Button::Rs => 0x0080,
            Button::Lb => 0x0100,
            Button::Rb => 0x0200,
            Button::Guide => 0x0400,
            Button::A => 0x1000,
            Button::B => 0x2000,
            Button::X => 0x4000,
            Button::Y => 0x8000,
        }
    }
}

pub const KEYS: [u16; 11] = [0x130, 0x131, 0x133, 0x134, 0x136, 0x137, 0x13a, 0x13b, 0x13c, 0x13d, 0x13e];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Lx,
    Ly,
    Rx,
    Ry,
    Lt,
    Rt,
}

impl Axis {
    pub fn parse(s: &str) -> Option<Axis> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "lx" => Axis::Lx,
            "ly" => Axis::Ly,
            "rx" => Axis::Rx,
            "ry" => Axis::Ry,
            "lt" | "l2" | "left_trigger" | "lefttrigger" => Axis::Lt,
            "rt" | "r2" | "right_trigger" | "righttrigger" => Axis::Rt,
            _ => return None,
        })
    }
    pub fn is_trigger(self) -> bool {
        matches!(self, Axis::Lt | Axis::Rt)
    }
    pub fn name(self) -> &'static str {
        match self {
            Axis::Lx => "lx",
            Axis::Ly => "ly",
            Axis::Rx => "rx",
            Axis::Ry => "ry",
            Axis::Lt => "lt",
            Axis::Rt => "rt",
        }
    }
}

/// One change to a pad.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    Press { button: Button },
    Release { button: Button },
    /// Sticks -1..1 (Y up), triggers 0..1; clamped.
    Axis { axis: Axis, value: f32 },
    /// Everything released and centred.
    Neutral,
}

/// What the pad is doing: in XInput's terms (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PadState {
    /// Bit `Button as u32` per pressed button.
    pub buttons: u32,
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    pub lt: f32,
    pub rt: f32,
}

impl PadState {
    pub fn pressed(&self, b: Button) -> bool {
        self.buttons & b.bit() != 0
    }
    pub fn pressed_list(&self) -> Vec<Button> {
        Button::ALL.into_iter().filter(|b| self.pressed(*b)).collect()
    }
    pub fn axis(&self, a: Axis) -> f32 {
        match a {
            Axis::Lx => self.lx,
            Axis::Ly => self.ly,
            Axis::Rx => self.rx,
            Axis::Ry => self.ry,
            Axis::Lt => self.lt,
            Axis::Rt => self.rt,
        }
    }
    fn axis_mut(&mut self, a: Axis) -> &mut f32 {
        match a {
            Axis::Lx => &mut self.lx,
            Axis::Ly => &mut self.ly,
            Axis::Rx => &mut self.rx,
            Axis::Ry => &mut self.ry,
            Axis::Lt => &mut self.lt,
            Axis::Rt => &mut self.rt,
        }
    }
    pub fn apply(&mut self, c: &Change) {
        match c {
            Change::Press { button } => self.buttons |= button.bit(),
            Change::Release { button } => self.buttons &= !button.bit(),
            Change::Axis { axis, value } => {
                let v = if value.is_finite() { *value } else { 0.0 };
                *self.axis_mut(*axis) = if axis.is_trigger() { v.clamp(0.0, 1.0) } else { v.clamp(-1.0, 1.0) };
            }
            Change::Neutral => *self = PadState::default(),
        }
    }

    /// XInput's view of this state: (wButtons, LT, RT, LX, LY, RX, RY), as a game would read it.
    pub fn xinput(&self) -> (u16, u8, u8, i16, i16, i16, i16) {
        let b = self.pressed_list().into_iter().fold(0u16, |acc, b| acc | b.xinput());
        (b, trigger(self.lt) as u8, trigger(self.rt) as u8, stick(self.lx), stick(self.ly), stick(self.rx), stick(self.ry))
    }

    /// The evdev value of every axis (ABS code, value), xpad's way.
    fn abs(&self) -> [(u16, i32); 8] {
        let hat = |neg: Button, pos: Button| match (self.pressed(neg), self.pressed(pos)) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        };
        [
            (ABS_X, stick(self.lx) as i32),
            (ABS_Y, !stick(self.ly) as i32),
            (ABS_Z, trigger(self.lt)),
            (ABS_RX, stick(self.rx) as i32),
            (ABS_RY, !stick(self.ry) as i32),
            (ABS_RZ, trigger(self.rt)),
            (ABS_HAT0X, hat(Button::Left, Button::Right)),
            (ABS_HAT0Y, hat(Button::Up, Button::Down)),
        ]
    }

    /// The events that take the device from `prev` to `self`, then SYN_REPORT; empty when
    /// nothing changed.
    pub fn events_from(&self, prev: &PadState) -> Vec<(u16, u16, i32)> {
        let mut ev = Vec::new();
        for b in Button::ALL {
            if let Some(k) = b.key()
                && self.pressed(b) != prev.pressed(b)
            {
                ev.push((EV_KEY, k, self.pressed(b) as i32));
            }
        }
        for ((code, v), (_, pv)) in self.abs().into_iter().zip(prev.abs()) {
            if v != pv {
                // SDL takes a trigger's first move from rest into the middle of its range
                // (|v| < 8192 of ±32767, raw 96..=160) for its real rest value and drops it
                // (SDL_SendJoystickAxis's initial-value heuristic). A real trigger passes through
                // lower values first; so does this one, in the same frame.
                if matches!(code, ABS_Z | ABS_RZ) && pv == 0 && (TRIGGER_SDL_BAND).contains(&v) {
                    ev.push((EV_ABS, code, TRIGGER_PRIMER));
                }
                ev.push((EV_ABS, code, v));
            }
        }
        if !ev.is_empty() {
            ev.push((EV_SYN, SYN_REPORT, 0));
        }
        ev
    }

    /// Every axis at its value, for a freshly made device (the kernel starts them at 0, which
    /// is centred for sticks and released for triggers, and so matches the default state).
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let p: Vec<&str> = self.pressed_list().iter().map(|b| b.name()).collect();
        parts.push(if p.is_empty() { "no buttons".into() } else { p.join("+") });
        for (n, x, y) in [("left stick", self.lx, self.ly), ("right stick", self.rx, self.ry)] {
            if x != 0.0 || y != 0.0 {
                parts.push(format!("{n} ({x:.2}, {y:.2})"));
            }
        }
        for (n, v) in [("LT", self.lt), ("RT", self.rt)] {
            if v != 0.0 {
                parts.push(format!("{n} {v:.2}"));
            }
        }
        parts.join(", ")
    }
}

/// Raw trigger values SDL would mistake for the rest position on a first move (see
/// [`PadState::events_from`]), and the value sent just before them.
pub const TRIGGER_SDL_BAND: std::ops::RangeInclusive<i32> = 96..=160;
pub const TRIGGER_PRIMER: i32 = 64;

/// The value an axis has when nothing touches it (xpad reports `!0 = -1` for a centred Y).
pub fn rest(code: u16) -> i32 {
    if matches!(code, ABS_Y | ABS_RY) { -1 } else { 0 }
}

/// -1..1 to XInput's thumb range (-32768..32767).
pub fn stick(v: f32) -> i16 {
    let v = v.clamp(-1.0, 1.0);
    if v >= 0.0 { (v * 32767.0).round() as i16 } else { (v * 32768.0).round().max(-32768.0) as i16 }
}

/// 0..1 to 0..255.
pub fn trigger(v: f32) -> i32 {
    (v.clamp(0.0, 1.0) * 255.0).round() as i32
}

/// The absolute axes the device declares: (code, min, max, fuzz, flat), as xpad does.
pub const ABS_AXES: [(u16, i32, i32, i32, i32); 8] = [
    (ABS_X, -32768, 32767, 16, 128),
    (ABS_Y, -32768, 32767, 16, 128),
    (ABS_Z, 0, 255, 0, 0),
    (ABS_RX, -32768, 32767, 16, 128),
    (ABS_RY, -32768, 32767, 16, 128),
    (ABS_RZ, 0, 255, 0, 0),
    (ABS_HAT0X, -1, 1, 0, 0),
    (ABS_HAT0Y, -1, 1, 0, 0),
];

/// A pad's evdev name: `Quiver Lab Pad <instance>`, then ` P<k>` for the second and later
/// (`k` from 1). The udev rule matches `Quiver Lab Pad *`.
pub fn pad_name(instance: &str, index: usize) -> String {
    let base = format!("Quiver Lab Pad {instance}");
    let name = if index == 0 { base } else { format!("{base} P{}", index + 1) };
    // UINPUT_MAX_NAME_SIZE is 80, with the NUL.
    name.chars().take(79).collect()
}

/// A pad's phys: `quiver-lab/<instance>/<index>`. The rule matches `quiver-lab/*`; the lab
/// finds an instance's pads (and stale ones) by it.
pub fn pad_phys(instance: &str, index: usize) -> String {
    format!("{}{instance}/{index}", PHYS_PREFIX)
}

pub const PHYS_PREFIX: &str = "quiver-lab/";
pub const NAME_PREFIX: &str = "Quiver Lab Pad ";

/// (instance, index) from a phys this lab gave a pad.
pub fn parse_phys(phys: &str) -> Option<(String, usize)> {
    let rest = phys.trim().strip_prefix(PHYS_PREFIX)?;
    let (inst, idx) = rest.rsplit_once('/')?;
    Some((inst.to_owned(), idx.parse().ok()?))
}

/// The hyprpad grips (Steam Controller L4, L5, R4, R5) as a hyprpad user's `pad_map` would put
/// them on an Xbox pad: `r4=a,r5=b,l4=up,l5=down`. Parsed from `GRIP=BUTTON` pairs.
pub fn parse_grip_map(s: &str) -> anyhow::Result<Vec<(String, Button)>> {
    let mut out = Vec::new();
    for pair in s.split([',', ' ']).filter(|p| !p.is_empty()) {
        let (g, b) = pair.split_once('=').ok_or_else(|| anyhow::anyhow!("'{pair}': expected GRIP=BUTTON (r4=a)"))?;
        let g = g.trim().to_ascii_lowercase();
        if !["l4", "l5", "r4", "r5"].contains(&g.as_str()) {
            anyhow::bail!("'{g}': the grips are l4, l5, r4 and r5");
        }
        let b = Button::parse(b).ok_or_else(|| anyhow::anyhow!("'{b}' isn't a pad button"))?;
        out.push((g, b));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_aliases() {
        for b in Button::ALL {
            assert_eq!(Button::parse(b.name()), Some(b));
        }
        assert_eq!(Button::parse("Cross"), Some(Button::A));
        assert_eq!(Button::parse("dpad-up"), Some(Button::Up));
        assert_eq!(Button::parse("L3"), Some(Button::Ls));
        assert_eq!(Button::parse("menu"), Some(Button::Start));
        assert_eq!(Button::parse("r4"), None);
        assert_eq!(Axis::parse("RT"), Some(Axis::Rt));
    }

    #[test]
    fn events_are_xpads_and_only_the_changes() {
        let mut s = PadState::default();
        assert!(s.events_from(&PadState::default()).is_empty());
        s.apply(&Change::Press { button: Button::A });
        assert_eq!(s.events_from(&PadState::default()), vec![(EV_KEY, 0x130, 1), (EV_SYN, SYN_REPORT, 0)]);
        let prev = s;
        // Up on the left stick is evdev's negative Y (xpad: !y).
        s.apply(&Change::Axis { axis: Axis::Ly, value: 1.0 });
        s.apply(&Change::Axis { axis: Axis::Rt, value: 2.0 });
        s.apply(&Change::Press { button: Button::Up });
        assert_eq!(s.events_from(&prev), vec![(EV_ABS, ABS_Y, -32768), (EV_ABS, ABS_RZ, 255), (EV_ABS, ABS_HAT0Y, -1), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(s.rt, 1.0, "clamped");
        // A trigger from rest straight to the middle passes through a lower value (SDL).
        let mut half = PadState::default();
        half.apply(&Change::Axis { axis: Axis::Lt, value: 0.5 });
        assert_eq!(half.events_from(&PadState::default()), vec![(EV_ABS, ABS_Z, TRIGGER_PRIMER), (EV_ABS, ABS_Z, 128), (EV_SYN, SYN_REPORT, 0)]);
        let mut more = half;
        more.apply(&Change::Axis { axis: Axis::Lt, value: 0.55 });
        assert_eq!(more.events_from(&half), vec![(EV_ABS, ABS_Z, 140), (EV_SYN, SYN_REPORT, 0)], "only from rest");
        let mut low = PadState::default();
        low.apply(&Change::Axis { axis: Axis::Rt, value: 0.25 });
        assert_eq!(low.events_from(&PadState::default()), vec![(EV_ABS, ABS_RZ, 64), (EV_SYN, SYN_REPORT, 0)], "outside the band: as is");
        assert_eq!((rest(ABS_Y), rest(ABS_RY), rest(ABS_X), rest(ABS_Z)), (-1, -1, 0, 0));
        assert_eq!(PadState::default().abs().iter().map(|(c, v)| (*c, *v)).filter(|(_, v)| *v != 0).collect::<Vec<_>>(), vec![(ABS_Y, -1), (ABS_RY, -1)], "the device starts at the model's rest");
        let prev = s;
        s.apply(&Change::Axis { axis: Axis::Ly, value: -1.0 });
        s.apply(&Change::Press { button: Button::Down });
        // Both up and down on the d-pad: the hat is centred.
        assert_eq!(s.events_from(&prev), vec![(EV_ABS, ABS_Y, 32767), (EV_ABS, ABS_HAT0Y, 0), (EV_SYN, SYN_REPORT, 0)]);
        let prev = s;
        s.apply(&Change::Neutral);
        let ev = s.events_from(&prev);
        assert!(ev.contains(&(EV_KEY, 0x130, 0)) && ev.contains(&(EV_ABS, ABS_Y, -1)) && ev.contains(&(EV_ABS, ABS_RZ, 0)));
    }

    #[test]
    fn xinput_view() {
        let mut s = PadState::default();
        for c in [
            Change::Press { button: Button::A },
            Change::Press { button: Button::Lb },
            Change::Press { button: Button::Left },
            Change::Axis { axis: Axis::Lx, value: -1.0 },
            Change::Axis { axis: Axis::Ry, value: 0.5 },
            Change::Axis { axis: Axis::Lt, value: 0.5 },
        ] {
            s.apply(&c);
        }
        assert_eq!(s.xinput(), (0x1000 | 0x0100 | 0x0004, 128, 0, -32768, 0, 0, 16384));
        assert_eq!(s.describe(), "a+lb+left, left stick (-1.00, 0.00), right stick (0.00, 0.50), LT 0.50");
    }

    #[test]
    fn names_and_phys() {
        assert_eq!(pad_name("awr", 0), "Quiver Lab Pad awr");
        assert_eq!(pad_name("awr", 1), "Quiver Lab Pad awr P2");
        assert!(pad_name(&"x".repeat(200), 0).len() < 80);
        assert_eq!(pad_phys("awr", 1), "quiver-lab/awr/1");
        assert_eq!(parse_phys("quiver-lab/awr-2/1"), Some(("awr-2".into(), 1)));
        assert_eq!(parse_phys("usb-0000:00:14.0-1/input0"), None);
    }

    #[test]
    fn grip_map() {
        let m = parse_grip_map("r4=a,r5=b,l4=dpad_up,l5=dpad_down").unwrap();
        assert_eq!(m, vec![("r4".into(), Button::A), ("r5".into(), Button::B), ("l4".into(), Button::Up), ("l5".into(), Button::Down)]);
        assert!(parse_grip_map("r6=a").is_err());
        assert!(parse_grip_map("r4=q").is_err());
    }
}
