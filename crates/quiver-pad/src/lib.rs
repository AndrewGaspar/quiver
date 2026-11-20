//! quiver lab's virtual game controllers (design/LAB.md §15).
//!
//! A lab instance can have pads: Linux input devices made with uinput on the host, laid out
//! as a wired Xbox 360 controller, that only that instance's container can open. Native Linux
//! games and Wine (winebus, through SDL) find them in the container's `/dev/input` as they
//! would a real one, so XInput, DirectInput, raw input and Windows.Gaming.Input games see a
//! controller, and so do SDL and evdev games.
//!
//! **Kept from the host.** A uinput device is global to the kernel, so the host would see it
//! too: Steam, the person's own games, hyprpad, the compositor. The udev rule
//! (`contrib/lab/72-quiver-lab-pad.rules`, [`rule`]) gives every lab pad's nodes a group no
//! host process has, no session ACL and no seat, and libinput ignores them. Pads are never made
//! unless that rule is installed, and each node is checked after udev is done with it; a pad
//! that isn't hidden is destroyed at once.
//!
//! **Pieces.** [`model`]: the controls, the state and its evdev events. [`uinput`]: the device
//! (with rumble). [`rule`]: the rule's checks and the gid mapping into rootless podman.
//! [`seq`]: scripts of presses, sticks and waits. [`host`]: the protocol of the process that
//! holds the devices. [`probe`]: the Windows program that reports what a game sees.

pub mod host;
pub mod model;
pub mod probe;
pub mod rule;
pub mod seq;
pub mod uinput;

pub use model::{Axis, Button, Change, PadState};
