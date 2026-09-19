# Changelog

All notable changes to vellum are documented here. The Rust rewrite starts a new version series rather than continuing the retired Python implementation's releases.

## [Unreleased]

### Added

- Restyle the whole settings panel into a modern dark desktop app: a three-layer material system (deep window base with a soft top light, translucent card surfaces with hairline borders and a top inset sheen, sunken input insets), an indigo accent (`#4f6ef7`) replacing the washed-out periwinkle, and status shown as a glowing dot in a low-saturation pill.
- Replace the panel's centred top tabs with the classic master-detail layout: a 184 px sidebar (模型接入 / 翻译与 OCR / 截图行为, mutually exclusive, keyboard-reachable) beside a content column whose forms are capped at 640 px, so a URL or proxy field no longer stretches across the window. `Ctrl+1`/`Ctrl+2`/`Ctrl+3` switch pages and keep the sidebar in sync.
- Rebuild every field as an Action Row — name and one-line description on the left, control on the right — with hairline separators between rows. New shared widgets live in `crates/vellum-ui/src/controls.rs`: action rows, cards, the embedded-eyebrow secret field, the compact stepper, secondary and primary buttons, status dots/pills and the segmented control.
- Move the API key reveal inside the field (a borderless eye toggle instead of a detached 显示 button beside it), lighten the steppers into one inset shell with the unit spelled out ("秒", "倍"), and drop the floppy-disk icon from the save button.
- Quiet the developer-facing copy: the `HTTPS_PROXY`/`ALL_PROXY`/`none` rules moved from a paragraph under the proxy field into its tooltip and placeholder, the "上面留空时读它" note is gone, and the title's redundant subtitle is removed.
- Style the settings controls the way GTK actually names them: a grouped `GtkCheckButton` exposes its indicator as `radio`, not `check`, so the OCR engine choice now renders as a clean segmented control instead of two leftover radio dots (a CSS rule targeting `check` silently matched nothing).
- Add a CSS regression test that parses the stylesheet through GTK and fails on any parsing error, plus a test asserting every class the panel and picker apply still has a rule in `theme.rs`; `CSS_VERSION` is bumped to 9.
- Add `install-remote.sh`: a `curl | bash` one-liner that stages the source in a temporary directory, runs the in-tree installer and removes every trace on exit.
- Name the translation path in the result window footer (which API model answered, or that the text was already in the target language), as `ARCHITECTURE.md` §2.4 requires.
- Add a settings panel (`vellum panel`, the tray menu entry "设置面板" and a desktop entry) that configures the model API, translation, the OCR engine and the capture preferences. Its header is a drag handle, so the window can be moved with the mouse.
- Add an OpenAI-compatible API client (`crates/vellum-text/src/api.rs`) shared by translation and API vision OCR, including a hand-written base64 encoder for image data URLs and a `GET /models` probe behind the panel's connection test.
- Add 「获取模型」 to the panel: it requests `/models` and fills two searchable model pickers (translation and OCR vision) that also accept free text. The pickers strip the endpoint's path prefix (`models/…`) for display but commit the exact id, dedupe repeats, ellipsize long ids with the full string in a tooltip, follow the entry's width, and drive a keyboard cursor with ↑/↓/Enter/Esc; a typed name that the endpoint never reported is called out as a hint rather than an error, because gateways do serve models their `/models` never lists.
- Restructure the panel: model fields live on the 模型接入 page, translation and OCR merged into one scrollable 翻译与 OCR page, section cards with a footer bar, thinner scrollbars, a subtle window sheen, radio-style engine buttons, `Ctrl+1`/`Ctrl+2` page shortcuts and `Ctrl+S` to save.
- Fix the model picker's first mouse-open showing an empty list: the refill was hung off `MenuButton::activate`, which a click never emits; it now runs on `popover::map`, which also measures the popover width.
- Add an HTTP proxy setting (`[api] proxy`, panel field): empty reads `HTTPS_PROXY`/`ALL_PROXY`/`HTTP_PROXY`, `none` forces a direct connection. A blocked endpoint (OpenAI, Google) previously failed as an opaque timeout with no way to route it through the user's own proxy.
- Report a reachable endpoint whose `/models` list does not contain the configured model (for example `gpt-4o-mini` against Google's OpenAI-compatible endpoint) instead of letting it fail later with a 404.
- Offer two OCR engines: the built-in offline Tesseract pipeline, or a vision model over the same API endpoint.
- Count text rows before choosing the single-line page segmentation: a small wrapped paragraph has the same box shape as a banner, and PSM 7 on it used to return nothing and pay for a second Tesseract start (measured 1.0 s -> 0.52 s on a clean three-line crop).
- Ship window rules for the panel in the niri/Hyprland examples, plus `ai.vellum-panel.desktop`.

### Changed

- Improve long-shot reconstruction across repeated content, reverse scrolling, history revisits, fixed regions and local animation without relaxing acceptance thresholds.
- Improve OCR preprocessing and candidate ranking for dim, colored, gradient and isoluminant text.
- `install.sh` now removes the cargo build tree after a successful install, since an end-user install never rebuilds; `VELLUM_SKIP_CLEANUP=1` keeps it for incremental rebuilds.
- Drop the `panic` override from `[profile.bench]`: cargo ignores it for bench targets and printed a warning on every benchmark run.
- Retry a transient `accept` failure a bounded number of times instead of exiting the control service, and document the measurement dates, cross-references, visual tokens and per-variable trace semantics the audits found stale.
- Translation is API-only: the `opencode serve`/`opencode run` backends are gone, and a legacy `opencode/` model prefix is stripped while loading so a migrated config does not 404. `[api]` is now the shared endpoint (base URL, key, key environment variable, timeout); `[ocr] engine` accepts `builtin`/`api` (the old `tesseract`/`vision` names still load).
- The configuration file can now be written by vellum: `Config::save` renders a commented document and replaces it atomically with mode 0600, because it may hold an API key.
- `vellum doctor` reports the configured model endpoint, model and key source instead of looking for an `opencode` binary.

### Fixed

- Install the long-shot finish signal handler before startup, ignore selection-stage presses and arm completion only after a valid region is confirmed.
- Require each screencopy frame to negotiate a supported current `wl_shm` buffer before `BufferDone`, preventing stale-buffer submission.
- Keep recorder controls and selection highlights outside sampled pixels, and fail closed when direct mode cannot expose a safe completion control.
- Treat a finish that beats the first frame (a second hotkey inside the overlay-to-recorder handoff, or 完成 during panel verification) as a user cancellation: it used to be reported as a capture failure, which fired a critical notification, exited 1 and made the daemon log a "startup failed (code 1)" event.
- Guard `systemctl --user daemon-reload` in `install.sh`: with an unreachable user manager (sudo, no session bus) the installer aborted after replacing the binaries but before the environment check and the build-tree cleanup.
- Update the locked `rustls` to 0.23.45 (and `rustls-webpki` to 0.103.15) to clear RUSTSEC-2026-0285, which the documented `cargo audit --no-yanked` gate flags as a medium-severity vulnerability. The workspace's direct-dependency pins are unchanged.
- Keep the persistent screencopy connection across a transient timeout: the move to grim is irreversible and costs a fork/exec per frame, so it now requires three consecutive timeouts (or a real protocol/geometry failure) instead of one.
- Stop vellum from floating a window that is not its own: the panel/pin/result float used to fall back to "float the focused window" when the compositor's client list had not caught up with the just-mapped window, which floated — and visibly shrank — whatever the user had focused (reported as "my browser shrank when I opened the panel"). The fallback is gone and the pid lookup is retried on the main loop instead; a miss now leaves the window tiled.
- Keep the tray alive on sessions that never activate `graphical-session.target` (this machine's Hyprland setup is one): the unit is now also installed into `default.target`, and the tray waits up to five minutes for the shell's `StatusNotifierWatcher` instead of dying inside systemd's restart budget. Without this, the tray — and with it the "设置面板" entry — never appeared after logging into Hyprland.
- Stop counting a daemon that is shutting down as available: `ping()` now requires `ok && running`, a non-accepted answer from a daemon that is not running falls back to an in-process action instead of dropping the keypress, and `vellum restart` waits for the old socket to stop answering before activating the new daemon.

### Testing

- Add `tests/install-systemd-unreachable.sh` (also run by CI): it drives the real installer with a stubbed toolchain and a failing `systemctl --user`, and fails if the install aborts before the environment check and the build-tree cleanup.
- Verify the previously environment-blocked Hyprland paths on 2026-09-18 now that the session runs Hyprland: compositor detection, shortcut discovery from the Lua config, pin window and settings panel self-floating without any user window rule, the tray's new panel entry activated through `com.canonical.dbusmenu.Event`, both ignored live tests (including the recorder zero-pixel fixture and its 130-frame damage-driven capture), and a translation round trip against a local OpenAI-compatible mock.
- Re-measure the documented gates on 2026-09-18 (niri 26.04, rustc 1.97.1): 101-frame stitch benchmark 129.1-143.9 ms opaque and 134.7-147.0 ms translucent, socket round trips p50 0.037-0.049 ms (ping) and 0.037-0.056 ms (status), niri screencopy plain copy 1.68-2.08 ms per 900x700 frame with damage-driven `capture()` pacing at the 144 Hz output cadence, long-shot trace overhead 7 ns/event disabled and 4-7.7 us per recorded frame enabled, both live ignored tests passing, and the OCR regression gate at 10/10 with 100% accuracy.

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
