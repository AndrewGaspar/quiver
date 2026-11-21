-- Quiver Sight: the in-game panel with a controller, through hyprpad (design/SIGHT.md §6, §14).
--
-- Quiver ships this; you add it to your own hyprpad config (Quiver never edits it).
-- `quiver sight hyprpad --snippet` prints this file. Written for hyprpad's Lua config as of
-- 2026-11 (h.mode / h.bind / h.button, ctx.layers:has, the io library); see the notes for the
-- two edits to lines you already have.
--
-- What it does:
--   1. A `quiver-sight` mode, active while the Sight panel is open. The panel is normally
--      drawn inside the game (the Sight layer composites it and the game keeps the keyboard
--      focus), so there is no surface to see: quiver-sight writes the state file
--      $XDG_RUNTIME_DIR/quiver/sight-panel ("open <pid> <game|desktop> <session>") while it's
--      open, and the mode checks it (and that the pid is alive, so a quiver-sight that died
--      doesn't hold the pad). The desktop panel (games the layer can't draw in) is a
--      layer-shell surface, namespace exactly `quiver-sight`, matched as before. The mode
--      doesn't forward, so the game's virtual pad goes neutral (held buttons released) while
--      the panel is open, and comes back when it closes.
--   2. In that mode the pad types keys: D-pad and left stick: arrows · A: Enter · B: Escape · X: x (reset
--      to default; Achievements: reveal) · Y: y (tap: pin to the quick menu; hold on a live
--      option: compare its old and new value until you let go) · LB / RB: Page Up / Page Down
--      (previous / next section) · LT / RT: Home / End (the section's tabs: Mods' and
--      Tweaks' pages) · R3 (right stick click): Tab (dock the panel as a column beside the
--      game, or centre it). The keys go to the focused window: the game, whose input
--      Quiver's preload hands to the panel while it's open (or the desktop panel's surface).
--      Held buttons hold the key, so the panel's own key repeat applies, and Y's hold is seen.
--   3. Guide or Guide+Start toggles Sight. It opens on the Quick tab.
--
-- When it switches: hyprpad re-runs mode rules on compositor events (focus, layers) and on
-- its periodic rescan (`process_rescan_ms`, 500 ms by default), which runs while a rule
-- mentions ctx.focus:process_tree_has (yours do). Opening the panel in the game changes no
-- window or layer, so the mode follows within that rescan interval;
-- `h.daemon { process_rescan_ms = 150 }` makes it quicker.
--
-- Where it goes: the mode must be declared BEFORE your game modes (first matching mode wins),
-- e.g. right after the cheat sheet / launcher / omarchy-ui modes:
--
--     dofile(os.getenv("HOME") .. "/code/ubisoft-linux/tools/hyprpad/quiver-sight.lua")
--
-- (or copy this file next to your config and dofile that copy; hyprpad reloads it with
-- `hyprpad reload`, and keeps the last good config if it fails to load).

local h = hyprpad

-- The panel's state file says it's open, by a quiver-sight that's alive.
local function quiver_sight_open()
  local dir = os.getenv("XDG_RUNTIME_DIR")
  if not dir then return false end
  local f = io.open(dir .. "/quiver/sight-panel", "r")
  if not f then return false end
  local line = f:read("*l") or ""
  f:close()
  local pid = line:match("^open (%d+)")
  if not pid then return false end
  local p = io.open("/proc/" .. pid .. "/stat", "r")
  if not p then return false end
  p:close()
  return true
end

-- 1. The mode (no `forward = true`: the game's pad is held neutral).
h.mode("quiver-sight").when(function(ctx)
  return ctx.layers:has("quiver-sight") or quiver_sight_open()
end)

-- 2. Buttons -> keys, only while the panel is up. hyprpad key names (evdev style).
local keys = {
  dpad_up = "up", dpad_down = "down", dpad_left = "left", dpad_right = "right",
  a = "enter", b = "escape", x = "x", y = "y",
  l1 = "pageup", r1 = "pagedown", l2 = "home", r2 = "end",
  r3 = "tab",
}
for button, key in pairs(keys) do
  h.button(button, "Quiver Sight: " .. key, h.key(key)):only_in("quiver-sight")
end
h.button("rpad_click", "Quiver Sight: click", h.mouse "left"):only_in("quiver-sight")

-- Current hyprpad supports bare stick flicks. A held flick repeats with
-- h.sticks { flick_repeat = true }. If your config also binds a bare flick,
-- hyprpad has one action per gesture: route that action by HYPRPAD_MODE.
for _, dir in ipairs({ "up", "down", "left", "right" }) do
  -- Older releases accept only Guide chords in h.bind.
  local supported, binding = pcall(h.bind, "lstick_" .. dir, "Quiver Sight: " .. dir, h.key(dir))
  if not supported then break end
  binding:only_in("quiver-sight")
end
-- Add "quiver-sight" to the only_in lists of your h.cursor and h.scroll
-- settings for right-trackpad pointing and left-trackpad scrolling.

-- 3. Use one toggle binding in both your game mode and quiver-sight so the same
-- button opens and closes Sight. Keep your existing binding if it already runs
-- "quiver sight toggle". For example, if Guide+Start is free:
--
-- h.bind("guide+menu", "Toggle Quiver Sight", h.exec "quiver sight toggle"):only_in("game", "quiver-sight")
--
-- Substitute the names of your game modes. If the chord already has a desktop
-- action, route the existing binding by HYPRPAD_MODE. Hyprpad uses one binding
-- per chord. In Sight, release Guide, choose with the D-pad, then press A.
