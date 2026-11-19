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
--   2. In that mode the pad types keys: D-pad / left stick: arrows · A: Enter · B: Escape ·
--      X: Space · LB / RB (and LT / RT): Page Up / Page Down (previous / next section). The
--      keys go to the focused window: the game, whose input Quiver's preload hands to the
--      panel while it's open (or the desktop panel's surface). Held buttons hold the key, so
--      the panel's own key repeat applies.
--   3. Guide+Start opens and closes the panel (`quiver sight toggle`). See "The chord" below:
--      Guide+Start is Guide+Menu, which your config may already use.
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
  a = "enter", b = "escape", x = "space",
  l1 = "pageup", r1 = "pagedown", l2 = "pageup", r2 = "pagedown",
}
for button, key in pairs(keys) do
  h.button(button, "Quiver Sight: " .. key, h.key(key)):only_in("quiver-sight")
end
-- Stick flicks (left/right only: lstick_up/down are often bound already, e.g. to paging;
-- add "quiver-sight" to those binds' modes, sending Up/Down, if you want them too).
h.bind("lstick_left", "Quiver Sight: left", h.key "left"):only_in("quiver-sight")
h.bind("lstick_right", "Quiver Sight: right", h.key "right"):only_in("quiver-sight")

-- 3. The chord. A guide chord has exactly ONE binding in hyprpad: binding it again replaces
-- the earlier one everywhere (and keeps the earlier guard). So pick one:
--
-- (a) Guide+Start for Quiver in games and over the panel, your existing action elsewhere.
--     Replace your `h.bind("guide+menu", ...)` line with one that runs a script branching on
--     the mode hyprpad passes in $HYPRPAD_MODE, e.g. ~/.config/hypr/scripts/hyprpad-start.sh:
--
--         #!/bin/sh
--         case "$HYPRPAD_MODE" in
--           game|game-*|quiver-sight) exec quiver sight toggle ;;
--           *) exec omarchy-menu ;;
--         esac
--
--     and:  h.bind("guide+menu", "Omarchy menu / Quiver Sight in game", h.exec "~/.config/hypr/scripts/hyprpad-start.sh")
--
-- (b) If nothing of yours uses Guide+Start, bind it directly (uncomment):
--
-- h.bind("guide+start", "Quiver Sight", h.exec "quiver sight toggle")
--
-- Also: a bare Guide tap runs your guide-tap binding in this mode (the Steam pulse only
-- happens while forwarding). If that opens a launcher, add "quiver-sight" to its :not_in(...)
-- so a tap over the panel doesn't open it on top.
