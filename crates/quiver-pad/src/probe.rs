//! `quiver-pad-probe.exe` (win/probe.c): what a Windows game in the instance sees, read in
//! the running game's wineserver session.

use serde::{Deserialize, Serialize};

use crate::model::{PRODUCT, VENDOR};

include!(concat!(env!("OUT_DIR"), "/probes.rs"));

/// The probe for a prefix architecture (`x64` or `x86`).
pub fn exe(arch: &str) -> Option<&'static [u8]> {
    PROBES.iter().find(|(a, _)| *a == arch).map(|(_, b)| *b)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct XSlot {
    pub slot: u32,
    pub connected: bool,
    #[serde(default)]
    pub packet: u32,
    #[serde(default)]
    pub buttons: u16,
    #[serde(default)]
    pub lt: u8,
    #[serde(default)]
    pub rt: u8,
    #[serde(default)]
    pub lx: i16,
    #[serde(default)]
    pub ly: i16,
    #[serde(default)]
    pub rx: i16,
    #[serde(default)]
    pub ry: i16,
    #[serde(rename = "type", default)]
    pub kind: Option<u8>,
    #[serde(default)]
    pub subtype: Option<u8>,
    #[serde(default)]
    pub flags: Option<u16>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DiState {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub rx: i32,
    pub ry: i32,
    pub rz: i32,
    pub pov: i32,
    pub pressed: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DiDevice {
    pub name: String,
    pub product: String,
    pub vid: u32,
    pub pid: u32,
    #[serde(rename = "type")]
    pub kind: u32,
    #[serde(default)]
    pub axes: Option<u32>,
    #[serde(default)]
    pub buttons: Option<u32>,
    #[serde(default)]
    pub povs: Option<u32>,
    #[serde(default)]
    pub state: Option<DiState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HidDevice {
    pub path: String,
    #[serde(default)]
    pub vid: Option<u32>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub usage_page: Option<u32>,
    #[serde(default)]
    pub usage: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RawDevice {
    pub name: String,
    #[serde(default)]
    pub vid: Option<u32>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub usage_page: Option<u32>,
    #[serde(default)]
    pub usage: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub xinput_dll: String,
    pub xinput: Vec<XSlot>,
    pub dinput: Vec<DiDevice>,
    pub hid: Vec<HidDevice>,
    pub rawinput: Vec<RawDevice>,
}

/// XInput subtypes (XINPUT_DEVSUBTYPE_*).
pub fn subtype_name(s: u8) -> &'static str {
    match s {
        1 => "gamepad",
        2 => "wheel",
        3 => "arcade stick",
        4 => "flight stick",
        5 => "dance pad",
        6 => "guitar",
        8 => "drum kit",
        _ => "other",
    }
}

impl Report {
    pub fn parse(text: &str) -> anyhow::Result<Report> {
        let line = text.lines().rev().find(|l| l.trim_start().starts_with('{')).ok_or_else(|| anyhow::anyhow!("the probe printed no report: {}", text.trim()))?;
        Ok(serde_json::from_str(line)?)
    }

    /// Lines for `quiver lab pad status`: what the game sees.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let slots: Vec<&XSlot> = self.xinput.iter().filter(|s| s.connected).collect();
        if self.xinput_dll.is_empty() {
            out.push("XInput: no xinput DLL loaded".into());
        } else if slots.is_empty() {
            out.push(format!("XInput ({}): no controller in slots 0-3", self.xinput_dll));
        }
        for s in slots {
            out.push(format!(
                "XInput slot {}: connected, {} ({}), packet {}, buttons {:#06x}, LT {} RT {}, left ({}, {}), right ({}, {})",
                s.slot,
                s.subtype.map(subtype_name).unwrap_or("?"),
                self.xinput_dll,
                s.packet,
                s.buttons,
                s.lt,
                s.rt,
                s.lx,
                s.ly,
                s.rx,
                s.ry
            ));
        }
        for d in &self.dinput {
            out.push(format!(
                "DirectInput: \"{}\" {:04x}:{:04x}, {} axes, {} buttons, {} POV{}",
                d.name,
                d.vid,
                d.pid,
                d.axes.unwrap_or(0),
                d.buttons.unwrap_or(0),
                d.povs.unwrap_or(0),
                d.state.as_ref().map(|s| format!(", buttons down {:#x}", s.pressed)).unwrap_or_default()
            ));
        }
        if self.dinput.is_empty() {
            out.push("DirectInput: no game controller".into());
        }
        let pads: Vec<&HidDevice> = self.hid.iter().filter(|h| h.usage_page == Some(1) && matches!(h.usage, Some(4) | Some(5))).collect();
        for h in &pads {
            out.push(format!("HID: \"{}\" {:04x}:{:04x} ({})", h.product.as_deref().unwrap_or("?"), h.vid.unwrap_or(0), h.pid.unwrap_or(0), h.path));
        }
        if pads.is_empty() {
            out.push(format!("HID: no gamepad or joystick ({} HID device(s))", self.hid.len()));
        }
        let raw = self.rawinput.iter().filter(|r| r.usage_page == Some(1) && matches!(r.usage, Some(4) | Some(5))).count();
        out.push(format!("raw input: {raw} gamepad/joystick device(s)"));
        out
    }

    /// The XInput slot showing an Xbox 360 pad (ours: the instance has no other).
    pub fn our_slot(&self) -> Option<&XSlot> {
        self.xinput.iter().find(|s| s.connected)
    }

    /// DirectInput sees an 045e:028e device.
    pub fn dinput_sees_xbox(&self) -> bool {
        self.dinput.iter().any(|d| d.vid == VENDOR as u32 && d.pid == PRODUCT as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_probes_json() {
        let text = r#"wine: some noise
{"xinput_dll":"xinput1_4.dll","xinput":[{"slot":0,"connected":true,"packet":12,"buttons":4096,"lt":0,"rt":255,"lx":0,"ly":32767,"rx":0,"ry":0,"type":1,"subtype":1,"flags":0},{"slot":1,"connected":false}],"dinput":[{"name":"Quiver Lab Pad awr","product":"Quiver Lab Pad awr","vid":1118,"pid":654,"type":66069,"axes":5,"buttons":11,"povs":1,"state":{"x":32767,"y":0,"z":0,"rx":0,"ry":0,"rz":0,"pov":-1,"pressed":1}}],"hid":[{"path":"\\\\?\\hid#vid_045e&pid_028e&ig_00","vid":1118,"pid":654,"version":276,"product":"Quiver Lab Pad awr","usage_page":1,"usage":5}],"rawinput":[{"name":"\\\\?\\HID#VID_045E&PID_028E&IG_00","vid":1118,"pid":654,"usage_page":1,"usage":5}]}
"#;
        let r = Report::parse(text).unwrap();
        assert_eq!(r.our_slot().unwrap().slot, 0);
        assert!(r.dinput_sees_xbox());
        let l = r.lines();
        assert!(l[0].starts_with("XInput slot 0: connected, gamepad (xinput1_4.dll), packet 12, buttons 0x1000, LT 0 RT 255, left (0, 32767)"), "{l:?}");
        assert!(l[1].contains("\"Quiver Lab Pad awr\" 045e:028e, 5 axes, 11 buttons, 1 POV"));
        assert!(l[2].starts_with("HID: \"Quiver Lab Pad awr\" 045e:028e"));
        assert_eq!(l[3], "raw input: 1 gamepad/joystick device(s)");
        let empty = Report::parse(r#"{"xinput_dll":"xinput1_4.dll","xinput":[{"slot":0,"connected":false}],"dinput":[],"hid":[],"rawinput":[]}"#).unwrap();
        assert_eq!(empty.lines()[0], "XInput (xinput1_4.dll): no controller in slots 0-3");
        assert!(Report::parse("nothing").is_err());
    }

    #[test]
    fn probes_are_embedded() {
        if PROBES.is_empty() {
            return; // built without mingw
        }
        let exe = exe("x64").expect("x64 probe");
        assert_eq!(&exe[..2], b"MZ");
    }
}
