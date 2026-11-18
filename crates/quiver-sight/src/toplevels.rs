//! The compositor's toplevel windows and which one is active, from
//! `zwlr_foreign_toplevel_manager_v1` (wlr-foreign-toplevel-management: Hyprland, Sway,
//! labwc, river, niri, Wayfire, …) on a Wayland connection of `quiver-sight`'s own, on its own
//! thread (GPUI's connection is GPUI's). Used to tie the Sight panel and the desktop HUD to the
//! game's window ([`crate::front`], design/SIGHT.md §14).
//!
//! The protocol has no pids or workspaces; what it has is enough: per toplevel its `app_id`,
//! title and state (`activated`, `minimized`, `fullscreen`), and `closed`. A workspace switch,
//! alt-tab, a click on another window, minimising (Omarchy moves the window to a special
//! workspace) all take `activated` from the game's toplevel. XWayland windows are toplevels
//! like any other.
//!
//! The thread sends a whole [`Event::Snapshot`] after each batch of events that changed
//! something (a batch is what one read brought, so a compositor's transient states inside one
//! change, as Hyprland's when it moves a fullscreen window, collapse into the final one), or
//! `Unavailable` once when the protocol isn't there.

use std::collections::HashMap;

use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, backend::ObjectId, event_created_child};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{zwlr_foreign_toplevel_handle_v1 as handle, zwlr_foreign_toplevel_manager_v1 as manager};

/// One toplevel. `id` is ours, stable for the toplevel's life and never reused in a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toplevel {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub activated: bool,
    pub minimized: bool,
    pub fullscreen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Every toplevel the compositor announced and finished describing (`done`).
    Snapshot(Vec<Toplevel>),
    /// No toplevel protocol (or it failed): why.
    Unavailable(String),
}

#[derive(Default)]
struct Entry {
    /// What `done` committed, and what the events since are building.
    shown: Option<Toplevel>,
    pending: Toplevel,
}

struct State {
    next_id: u64,
    entries: HashMap<ObjectId, Entry>,
    manager: bool,
    finished: bool,
    changed: bool,
}

impl State {
    fn snapshot(&self) -> Vec<Toplevel> {
        let mut v: Vec<Toplevel> = self.entries.values().filter_map(|e| e.shown.clone()).collect();
        v.sort_by_key(|t| t.id);
        v
    }
}

/// `zwlr_foreign_toplevel_handle_v1.state` values (0 is `maximized`).
const STATE_MINIMIZED: u32 = 1;
const STATE_ACTIVATED: u32 = 2;
const STATE_FULLSCREEN: u32 = 3;

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e
            && interface == manager::ZwlrForeignToplevelManagerV1::interface().name
            && !st.manager
        {
            st.manager = true;
            let _: manager::ZwlrForeignToplevelManagerV1 = r.bind(name, version.min(3), qh, ());
        }
    }
}

impl Dispatch<manager::ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(st: &mut Self, _: &manager::ZwlrForeignToplevelManagerV1, e: manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            manager::Event::Toplevel { toplevel } => {
                st.next_id += 1;
                let id = st.next_id;
                st.entries.insert(toplevel.id(), Entry { shown: None, pending: Toplevel { id, ..Default::default() } });
            }
            manager::Event::Finished => st.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, manager::ZwlrForeignToplevelManagerV1, [manager::EVT_TOPLEVEL_OPCODE => (handle::ZwlrForeignToplevelHandleV1, ())]);
}

/// The `state` array: native-endian u32s.
pub fn states(raw: &[u8]) -> (bool, bool, bool) {
    let mut out = (false, false, false);
    for c in raw.chunks_exact(4) {
        match u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) {
            STATE_ACTIVATED => out.0 = true,
            STATE_MINIMIZED => out.1 = true,
            STATE_FULLSCREEN => out.2 = true,
            _ => {}
        }
    }
    out
}

impl Dispatch<handle::ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(st: &mut Self, h: &handle::ZwlrForeignToplevelHandleV1, e: handle::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let key = h.id();
        if let handle::Event::Closed = e {
            if st.entries.remove(&key).is_some_and(|e| e.shown.is_some()) {
                st.changed = true;
            }
            h.destroy();
            return;
        }
        let Some(ent) = st.entries.get_mut(&key) else { return };
        match e {
            handle::Event::Title { title } => ent.pending.title = title,
            handle::Event::AppId { app_id } => ent.pending.app_id = app_id,
            handle::Event::State { state } => {
                (ent.pending.activated, ent.pending.minimized, ent.pending.fullscreen) = states(&state);
            }
            handle::Event::Done if ent.shown.as_ref() != Some(&ent.pending) => {
                ent.shown = Some(ent.pending.clone());
                st.changed = true;
            }
            _ => {}
        }
    }
}

/// Runs on the calling thread until the connection ends: `send` gets a snapshot after each
/// batch that changed something, or `Unavailable` once.
pub fn run(send: impl Fn(Event)) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => return send(Event::Unavailable(format!("no Wayland connection for the toplevel list ({e})"))),
    };
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State { next_id: 0, entries: HashMap::new(), manager: false, finished: false, changed: false };
    if let Err(e) = q.roundtrip(&mut st) {
        return send(Event::Unavailable(format!("the toplevel list's connection failed ({e})")));
    }
    if !st.manager {
        return send(Event::Unavailable("the compositor has no wlr-foreign-toplevel-management".into()));
    }
    // The toplevels that exist now, described.
    if let Err(e) = q.roundtrip(&mut st) {
        return send(Event::Unavailable(format!("the toplevel list's connection failed ({e})")));
    }
    st.changed = false;
    send(Event::Snapshot(st.snapshot()));
    loop {
        if let Err(e) = q.blocking_dispatch(&mut st) {
            return send(Event::Unavailable(format!("the toplevel list's connection failed ({e})")));
        }
        if st.finished {
            return send(Event::Unavailable("the compositor stopped the toplevel list".into()));
        }
        if std::mem::take(&mut st.changed) {
            send(Event::Snapshot(st.snapshot()));
        }
    }
}

/// [`run`] on a thread of its own.
pub fn spawn(send: impl Fn(Event) + Send + 'static) {
    let r = std::thread::Builder::new().name("quiver-sight-toplevels".into()).spawn(move || run(send));
    if let Err(e) = r {
        tracing::warn!("no toplevel thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_array() {
        // maximized (0), activated, fullscreen, and a value from a later version.
        let raw: Vec<u8> = [0, STATE_ACTIVATED, STATE_FULLSCREEN, 99].iter().flat_map(|s: &u32| s.to_ne_bytes()).collect();
        assert_eq!(states(&raw), (true, false, true));
        let raw: Vec<u8> = STATE_MINIMIZED.to_ne_bytes().to_vec();
        assert_eq!(states(&raw), (false, true, false));
        assert_eq!(states(&[1, 2, 3]), (false, false, false), "a short array is nothing");
    }
}
