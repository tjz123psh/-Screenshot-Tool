# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Bounded lossless keyframe storage and end-of-capture global path rebuilding for
  long screenshots, with a validated online-result fallback.
- Multi-frame detection of viewport-fixed headers, footers, and sidebars, which
  are excluded from overlap matching and retained only once in the final image.

## [0.2.0] - 2026-07-20

### Added

- Region capture with selection movement, resizing, save, and clipboard actions.
- Pen, arrow, rectangle, and text annotations with color, width, and undo controls.
- Local Tesseract OCR, optional vision OCR, and OpenCode-backed translation.
- Bidirectional long screenshot capture with continuous sampling and compact controls.
- Floating pinned screenshots with move, zoom, copy, and save actions.
- Traditional system tray menu, Unix socket control service, and lightweight hotkey client.
- Safe Niri shortcut discovery, installation, validation, rollback, and diagnostics.
- Idempotent Arch Linux installer for dependencies, launchers, icons, and user services.
- Arch Linux continuous integration for tests, compilation, lint, and syntax checks.

### Changed

- Long screenshot stitching can recover through recent frame history and tolerate local
  animation while rejecting unrelated content with pixel-level verification.
- Completing a long screenshot flushes queued and in-flight frames to preserve its tail.

[Unreleased]: https://github.com/tjz123psh/-Screenshot-Tool/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/tjz123psh/-Screenshot-Tool/releases/tag/v0.2.0
