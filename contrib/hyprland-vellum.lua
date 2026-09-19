-- Example Hyprland configuration for vellum (Lua config format).
--
-- This file is never read automatically, and `vellum shortcuts install` will
-- never write a Lua config: Hyprland's Lua format is code, not settings, and
-- this build refuses `hyprctl keyword` ("keyword can't work with non-legacy
-- parsers"), so there is no way to validate a generated edit before it takes
-- effect. Editing it is your call; copy the parts you want.
--
-- `vellum shortcuts install` prints the binds below ready to paste. If your
-- Hyprland uses the classic ini-style config, see hyprland-vellum.conf instead,
-- which vellum *can* write for you (backup, reload, configerrors, rollback).

-- Window rules. Add these to your windowrules.lua.
--
-- The pin window and the OCR/translation result window are reference overlays:
-- they should float above the tiling layout instead of taking a tile.
--
-- Both windows also ask the compositor to float them from code (~60 ms after
-- map), so these rules are optional. Keeping them removes the brief tiled flash
-- before that call lands, and lets you pin down a default size.
--
-- Match on class, not title: Hyprland's `class` is the GTK app-id, which is
-- stable, while the titles are localized ("vellum 钉图", "提取的文字") and would
-- break the moment the UI language changes.
--
-- Hyprland has no separate "no border" effect, so border_size = 0 is the way to
-- drop the compositor border (the pin window draws its own inner outline).
hl.window_rule({
	name = "float-vellum-pin",
	match = { class = "^ai\\.vellum\\.pin$" },
	float = true,
	border_size = 0,
})

hl.window_rule({
	name = "float-vellum-result",
	match = { class = "^ai\\.vellum\\.result$" },
	float = true,
	size = { 560, 400 },
})

hl.window_rule({
	name = "float-vellum-panel",
	match = { class = "^ai\\.vellum\\.panel$" },
	float = true,
	size = { 820, 640 },
})

-- Keybindings. Add these to your keybinds.lua.
--
-- Absolute path on purpose: ~/.local/bin is not on the PATH the compositor
-- spawns with, so a bare `vellumctl` would fail silently.
--
-- These spawn `vellumctl`, the thin hotkey client. It talks to the control
-- service over a Unix socket and only falls back to the full CLI if that fails,
-- which is what keeps the hotkey path free of GTK startup cost.
--
-- SUPER + Print is chosen to leave the bare Print / ALT+Print / CTRL+Print keys
-- to whatever already owns them.
hl.bind("SUPER + Print", hl.dsp.exec_cmd("$HOME/.local/bin/vellumctl region")) -- vellum 框选
hl.bind("SUPER + SHIFT + Print", hl.dsp.exec_cmd("$HOME/.local/bin/vellumctl long")) -- vellum 长截图
hl.bind("SUPER + CTRL + Print", hl.dsp.exec_cmd("$HOME/.local/bin/vellumctl pin-last")) -- vellum 钉图

-- The stage-1 selection overlay, the long-shot panel and the selection
-- highlight are layer-shell surfaces (namespaces vellum-overlay,
-- vellum-longshot, vellum-longshot-highlight), not toplevel windows, so they
-- need no window rule at all. Use `hyprctl layers` to confirm they map.
