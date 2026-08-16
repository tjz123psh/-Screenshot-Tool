# Changelog

All notable changes to vellum are documented here. The Rust rewrite starts a new version series rather than continuing the retired Python implementation's releases.

## [Unreleased]

### Added

- Add a persistent `wlr-screencopy` long-shot backend with bounded `grim` fallback, adaptive full/compact/micro controls and privacy-safe diagnostics.
- Add `install-remote.sh`: a `curl | bash` one-liner that stages the source in a temporary directory, runs the in-tree installer and removes every trace on exit.

### Changed

- Improve long-shot reconstruction across repeated content, reverse scrolling, history revisits, fixed regions and local animation without relaxing acceptance thresholds.
- Improve OCR preprocessing and candidate ranking for dim, colored, gradient and isoluminant text.
- `install.sh` now removes the cargo build tree after a successful install, since an end-user install never rebuilds; `VELLUM_SKIP_CLEANUP=1` keeps it for incremental rebuilds.

### Fixed

- Install the long-shot finish signal handler before startup, ignore selection-stage presses and arm completion only after a valid region is confirmed.
- Require each screencopy frame to negotiate a supported current `wl_shm` buffer before `BufferDone`, preventing stale-buffer submission.
- Keep recorder controls and selection highlights outside sampled pixels, and fail closed when direct mode cannot expose a safe completion control.

## [0.1.2] - 2026-08-01

### Fixed

- Serialize daemon completion, launch and shutdown so finished actions always run cursor/event/notification cleanup and no request can start after shutdown begins.
- Reap killed, detached and notification children instead of accumulating zombies in the daemon or tray.
- Reject non-executable PATH shadows, overflowing PPM dimensions and failed `pacman -T` dependency queries.
- Replace writable daemon test executables with stable shell fixtures to remove the parallel `ETXTBSY` race.

### Testing

- Add a deterministic synthetic OCR generator and scorer that uses only generated images and a developer-only local probe.
- Exercise installer query failures in isolation before cargo or user-file changes can run.

## [0.1.1] - 2026-08-01

### Changed

- Local OCR now adapts to faded text, uneven light/dark backgrounds, colored interference and isoluminant foreground/background colors using lazy CLAHE, polarity and color-projection candidates.
- Tesseract TSV confidence now drives preprocessing/layout selection, weak sparse-edge noise removal and per-line mixed-language fusion.

### Fixed

- Drain child stdout/stderr while writing stdin so large Tesseract or OpenCode output cannot deadlock and be misreported as a timeout; isolate their process groups so forked descendants cannot keep inherited pipes alive past the deadline.
- Keep sparse-noise filtering and mixed-language line fusion safe for multi-column text instead of dropping or swapping neighboring columns.

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

[Unreleased]: https://github.com/tjz123psh/-Screenshot-Tool/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/tjz123psh/-Screenshot-Tool/releases/tag/v0.1.2
[0.1.1]: https://github.com/tjz123psh/-Screenshot-Tool/releases/tag/v0.1.1
[0.1.0]: https://github.com/tjz123psh/-Screenshot-Tool/releases/tag/v0.1.0
