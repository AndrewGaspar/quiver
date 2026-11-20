//! The pad host's protocol: one JSON request line, one reply line, over
//! `<instance>/pad/host.sock`. The pad host (`quiver lab pad-host`) owns the uinput fds; the
//! CLI never touches the devices (it can't open their nodes: that's the point).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{Change, PadState};
use crate::rule::{NodeCheck, Plan};
use crate::uinput::{Nodes, Rumble};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Every pad's state.
    Get,
    /// Apply changes to one pad (0-based), in order, as one evdev frame.
    Apply { pad: usize, changes: Vec<Change> },
    /// Destroy the pads and exit.
    Quit,
}

/// One pad, as the host reports it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PadInfo {
    pub index: usize,
    pub name: String,
    pub phys: String,
    pub nodes: Nodes,
    pub state: PadState,
    pub rumble: Rumble,
    /// Event frames written since the pad was made.
    pub frames: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub pads: Vec<PadInfo>,
}

/// What `<instance>/pad/host.json` says: written once the pads exist and udev hid them (or
/// why not). `up` waits for it before it starts the container.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Ready {
    pub pid: u32,
    pub instance: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub plan: Option<Plan>,
    #[serde(default)]
    pub pads: Vec<ReadyPad>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReadyPad {
    pub index: usize,
    pub name: String,
    pub phys: String,
    pub nodes: Nodes,
    pub checks: Vec<NodeCheck>,
}

impl Ready {
    /// The host nodes to pass into the container.
    pub fn devices(&self) -> Vec<PathBuf> {
        self.pads.iter().flat_map(|p| std::iter::once(p.nodes.event.clone()).chain(p.nodes.js.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Button;

    #[test]
    fn requests_on_the_wire() {
        let r = Request::Apply { pad: 1, changes: vec![Change::Press { button: Button::A }, Change::Axis { axis: crate::model::Axis::Lx, value: 0.5 }, Change::Neutral] };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"op":"apply","pad":1,"changes":[{"op":"press","button":"a"},{"op":"axis","axis":"lx","value":0.5},{"op":"neutral"}]}"#);
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        assert_eq!(serde_json::from_str::<Request>(r#"{"op":"get"}"#).unwrap(), Request::Get);
    }

    #[test]
    fn ready_devices() {
        let n = |e: &str, j: Option<&str>| Nodes { sysname: "input1".into(), event: e.into(), js: j.map(Into::into) };
        let r = Ready {
            pid: 1,
            instance: "a".into(),
            error: None,
            plan: None,
            pads: vec![
                ReadyPad { index: 0, name: "n".into(), phys: "p".into(), nodes: n("/dev/input/event20", Some("/dev/input/js2")), checks: vec![] },
                ReadyPad { index: 1, name: "n".into(), phys: "p".into(), nodes: n("/dev/input/event21", None), checks: vec![] },
            ],
        };
        assert_eq!(r.devices(), vec![PathBuf::from("/dev/input/event20"), "/dev/input/js2".into(), "/dev/input/event21".into()]);
    }
}
