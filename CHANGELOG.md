# Changelog

All notable changes to vellum are documented here. The Rust rewrite starts a new version series rather than continuing the retired Python implementation's releases.

## [0.1.0] - 2026-07-31

### Added

- Complete Rust implementation split into seven crates and four purpose-built binaries.
- Region capture, annotation, local/vision OCR, translation, pin windows and a traditional dbusmenu system tray.
- Bidirectional manual-scroll long screenshots with fixed-region detection, local-animation rejection, bounded keyframes and validated offline rebuilding.
- Native niri and Hyprland window control, including floating pin/result windows and compositor-specific cursor handling.
- Lightweight JSON-over-Unix-socket control service and `vellumctl` hotkey client.
- Idempotent Arch Linux user installer, desktop metadata, icons, systemd user units and safe compositor shortcut integration.

### Performance

- Approximately 0.15 seconds for the 101-frame 900×700 long-shot benchmark.
- Approximately 0.04 milliseconds p50 for local control-socket ping/status round trips.

[0.1.0]: https://github.com/tjz123psh/-Screenshot-Tool/releases/tag/v0.1.0
