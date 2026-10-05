#!/usr/bin/env python3
"""Package already-built, trusted Vellum binaries; never build or install anything.

The binary allowlist is deliberate: source trees, user settings, screenshots and
recovery assets are not release resources. A checksum detects corruption, not an
untrusted publisher. Only --release-tag enables a clean tag release candidate;
all other archives are visibly labelled development bundles, including clean ones.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import selectors
import signal
import stat
import subprocess
import tarfile
import tempfile
import time

BINARIES = ("vellum", "vellumctl", "vellum-ui", "vellum-tray")
BUILD_FORMAT = "vellum-build-info-v1"
RELEASE_FORMAT = "vellum-release-v1"
BUILD_FIELDS = {
    "format", "version", "build_id", "source_commit", "source_dirty",
    "source_digest", "target", "rustc", "profile", "config_schema", "ipc_schema",
}
MAX_BINARY_BYTES = 512 * 1024 * 1024
MAX_RESOURCE_BYTES = 4 * 1024 * 1024
MAX_METADATA_BYTES = 64 * 1024
# Never glob contrib: compositor bindings and future unrelated assets are excluded.
RESOURCES = {
    "contrib/ai.vellum.desktop": "resources/applications/ai.vellum.desktop",
    "contrib/ai.vellum-panel.desktop": "resources/applications/ai.vellum-panel.desktop",
    "contrib/ai.vellum-shortcuts.desktop": "resources/autostart/ai.vellum-shortcuts.desktop",
    "contrib/ai.vellum.Shortcuts.service": "resources/dbus-1/services/ai.vellum.Shortcuts.service",
    "contrib/vellum.service": "resources/systemd/user/vellum.service",
    "contrib/vellum-tray.service": "resources/systemd/user/vellum-tray.service",
    "contrib/vellum-shortcuts.service": "resources/systemd/user/vellum-shortcuts.service",
    "contrib/icons/ai.vellum.svg": "resources/icons/hicolor/scalable/apps/ai.vellum.svg",
    "contrib/icons/ai.vellum-symbolic.svg": "resources/icons/hicolor/scalable/status/ai.vellum-symbolic.svg",
    "contrib/icons/ai.vellum-recording-symbolic.svg": "resources/icons/hicolor/scalable/status/ai.vellum-recording-symbolic.svg",
    "contrib/icons/ai.vellum-warning-symbolic.svg": "resources/icons/hicolor/scalable/status/ai.vellum-warning-symbolic.svg",
    "LICENSE": "LICENSE",
    "contrib/install-bundle.sh": "install.sh",
}


class PackageError(Exception):
    """A safe diagnostic; never include child stdout/stderr or environment values."""


def safe_component(value: object, maximum: int = 96) -> bool:
    return (isinstance(value, str) and 0 < len(value) <= maximum
            and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", value) is not None
            and value not in (".", ".."))


def safe_relative(value: str) -> bool:
    path = PurePosixPath(value)
    return (bool(value) and not path.is_absolute() and "\\" not in value
            and all(part not in ("", ".", "..") for part in value.split("/"))
            and not any(ord(char) < 32 or ord(char) == 127 for char in value))


def no_duplicate_keys(pairs: list[tuple[str, object]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise PackageError("duplicate JSON field")
        result[key] = value
    return result


def validate_build(info: object) -> dict:
    if not isinstance(info, dict) or set(info) != BUILD_FIELDS:
        raise PackageError("build information has unexpected or missing fields")
    if info["format"] != BUILD_FORMAT or not safe_component(info["build_id"]):
        raise PackageError("invalid build format or unsafe build ID")
    if not isinstance(info["version"], str) or not re.fullmatch(
            r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9._-]+)?", info["version"]):
        raise PackageError("invalid build version")
    if type(info["source_dirty"]) is not bool and info["source_dirty"] != "unknown":
        raise PackageError("source_dirty must be a boolean or explicit unknown")
    for field, pattern in (("source_commit", r"[0-9a-f]{40}|[0-9a-f]{64}|unknown"),
                           ("source_digest", r"[0-9a-f]{64}|unknown")):
        if not isinstance(info[field], str) or not re.fullmatch(pattern, info[field]):
            raise PackageError("invalid source identity")
    for field in ("target", "rustc", "profile"):
        value = info[field]
        if (not isinstance(value, str) or not 0 < len(value) <= 200
                or any(ord(c) < 32 or ord(c) > 126 or c in "/\\" for c in value)):
            raise PackageError("invalid build toolchain metadata")
    for field in ("config_schema", "ipc_schema"):
        if type(info[field]) is not int or info[field] != 1:
            raise PackageError("unsupported configuration or IPC schema")
    return info


def checked_file(root: Path, relative: str, executable: bool, limit: int) -> Path:
    if not safe_relative(relative):
        raise PackageError("unsafe package resource path")
    current = root
    if current.is_symlink() or not current.is_dir():
        raise PackageError("package input root must be a real directory")
    parts = relative.split("/")
    for index, part in enumerate(parts):
        current = current / part
        try:
            metadata = current.lstat()
        except OSError as error:
            raise PackageError("required package input is missing") from error
        if stat.S_ISLNK(metadata.st_mode):
            raise PackageError("symlinks are not allowed in package inputs")
        if index < len(parts) - 1:
            if not stat.S_ISDIR(metadata.st_mode):
                raise PackageError("resource parent is not a directory")
        elif not stat.S_ISREG(metadata.st_mode) or metadata.st_size > limit:
            raise PackageError("package input is not a bounded regular file")
        elif executable and not metadata.st_mode & 0o111:
            raise PackageError("package binary or installer is not executable")
    return current


def copy_member(root: Path, relative: str, bundle: Path, destination: str,
                executable: bool, limit: int) -> Path:
    source = checked_file(root, relative, executable, limit)
    target = bundle / destination
    target.parent.mkdir(parents=True, exist_ok=True)
    # Read with O_NOFOLLOW, then copy bytes rather than linking mutable build outputs.
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(descriptor, "rb") as reader, target.open("xb") as writer:
        meta = os.fstat(reader.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_size > limit:
            raise PackageError("package input changed while being copied")
        copied = 0
        while chunk := reader.read(1024 * 1024):
            copied += len(chunk)
            if copied > limit:
                raise PackageError("package input grew beyond its size limit")
            writer.write(chunk)
    target.chmod(0o755 if executable else 0o644)
    return target


def contains_marker(binary: Path) -> bool:
    marker = BUILD_FORMAT.encode("ascii")
    tail = b""
    with binary.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            joined = tail + chunk
            if marker in joined:
                return True
            tail = joined[-len(marker):]
    return False


def probe_build(binary: Path, timeout: float) -> dict:
    if not contains_marker(binary):
        raise PackageError("binary lacks the build-info marker; refusing to execute it")
    # This is not a sandbox for malicious binaries. Only probe trusted build outputs.
    # A minimal environment also prevents accidental access to the desktop/config.
    with tempfile.TemporaryDirectory(prefix="vellum-build-probe-") as scratch:
        env = {"PATH": "/usr/bin:/bin", "HOME": scratch, "XDG_CONFIG_HOME": scratch,
               "XDG_STATE_HOME": scratch, "XDG_CACHE_HOME": scratch,
               "XDG_RUNTIME_DIR": scratch, "LANG": "C", "LC_ALL": "C"}
        process = subprocess.Popen([str(binary), "--build-info-json"], cwd=scratch,
                                   env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        output = bytearray()
        error_bytes = 0
        deadline = time.monotonic() + timeout
        try:
            with selectors.DefaultSelector() as selector:
                for stream in (process.stdout, process.stderr):
                    os.set_blocking(stream.fileno(), False)
                    selector.register(stream, selectors.EVENT_READ)
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise PackageError("build-info probe timed out")
                    for key, _ in selector.select(min(remaining, 0.1)):
                        chunk = os.read(key.fileobj.fileno(), 8192)
                        if not chunk:
                            selector.unregister(key.fileobj)
                            continue
                        if key.fileobj is process.stdout:
                            output.extend(chunk)
                            if len(output) > MAX_METADATA_BYTES:
                                raise PackageError("build-info output exceeds its limit")
                        else:
                            error_bytes += len(chunk)
                            if error_bytes > 8192:
                                raise PackageError("build-info diagnostic output exceeds its limit")
                try:
                    code = process.wait(timeout=max(0.001, deadline - time.monotonic()))
                except subprocess.TimeoutExpired as error:
                    raise PackageError("build-info probe timed out") from error
                if code != 0:
                    raise PackageError("build-info probe failed")
        finally:
            # Reap the child and stop descendants that retained pipes or outlived it.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            process.stdout.close()
            process.stderr.close()
        try:
            info = json.loads(output, object_pairs_hook=no_duplicate_keys)
        except (ValueError, UnicodeError) as error:
            raise PackageError("build-info probe returned invalid JSON") from error
        return validate_build(info)


def git_output(source: Path, *args: str) -> str:
    try:
        result = subprocess.run(["git", "-C", str(source), *args], check=True,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                timeout=5)
        return result.stdout.decode("ascii").strip()
    except (OSError, subprocess.SubprocessError, UnicodeError) as error:
        raise PackageError("cannot verify clean tagged source checkout") from error


def verify_release_tag(source: Path, build: dict, tag: str | None) -> bool:
    if tag is None:
        return False
    if tag != "v" + build["version"] or not safe_component(tag):
        raise PackageError("release tag must be v followed by the exact build version")
    if (build["source_dirty"] is not False or build["source_commit"] == "unknown"
            or build["source_digest"] == "unknown" or build["profile"] != "release"):
        raise PackageError("official candidates require a known clean release build")
    head = git_output(source, "rev-parse", "HEAD")
    tagged = git_output(source, "rev-parse", "--verify", "refs/tags/" + tag + "^{commit}")
    if head != tagged or head != build["source_commit"]:
        raise PackageError("build source does not match the fixed release tag")
    if git_output(source, "status", "--porcelain", "--untracked-files=all"):
        raise PackageError("release source checkout has uncommitted files")
    return True


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, ensure_ascii=True, sort_keys=True, indent=2) + "\n").encode()


def digest_file(path: Path) -> str:
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            checksum.update(chunk)
    return checksum.hexdigest()


def write_manifest(bundle: Path, build: dict) -> dict:
    files = []
    for member in sorted(bundle.rglob("*")):
        metadata = member.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            continue
        relative = member.relative_to(bundle).as_posix()
        if not stat.S_ISREG(metadata.st_mode) or not safe_relative(relative):
            raise PackageError("unexpected package member")
        files.append({"path": relative, "sha256": digest_file(member),
                      "size": metadata.st_size, "executable": bool(metadata.st_mode & 0o111)})
    manifest = {"format": RELEASE_FORMAT, "release_id": build["build_id"],
                "version": build["version"], "build": build, "files": files}
    (bundle / "manifest.json").write_bytes(json_bytes(manifest))
    (bundle / "manifest.json").chmod(0o644)
    return manifest


def archive_bundle(bundle: Path, archive: Path, top: str, epoch: int) -> None:
    with archive.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path in [bundle, *sorted(bundle.rglob("*"))]:
                    relative = path.relative_to(bundle).as_posix()
                    name = top if relative == "." else top + "/" + relative
                    info = tarfile.TarInfo(name)
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    info.mtime = epoch
                    if path.is_dir():
                        info.type = tarfile.DIRTYPE
                        info.mode = 0o755
                        tar.addfile(info)
                    else:
                        info.size = path.stat().st_size
                        info.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
                        with path.open("rb") as stream:
                            tar.addfile(info, stream)
        raw.flush()
        os.fsync(raw.fileno())


def sync_directory(path: Path) -> None:
    directory = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def package(source: Path, binaries: Path, output: Path, tag: str | None = None,
            epoch: int = 0, timeout: float = 3.0, archive_path: Path | None = None) -> dict:
    source, binaries, output = (Path(os.path.abspath(p)) for p in (source, binaries, output))
    if not 0 <= epoch <= 2**32 - 1 or not 0.05 <= timeout <= 30:
        raise PackageError("invalid archive epoch or probe deadline")
    if output.is_symlink() or (output.exists() and (not output.is_dir() or any(output.iterdir()))):
        raise PackageError("output must be a new or empty directory; existing content is never replaced")
    output.parent.mkdir(parents=True, exist_ok=True)
    if archive_path is not None:
        archive_path = Path(os.path.abspath(archive_path))
        if archive_path.is_relative_to(output) or not archive_path.name.endswith(".tar.gz"):
            raise PackageError("archive must be a .tar.gz path outside the bundle directory")
        checksum_path = Path(str(archive_path) + ".sha256")
        for target in (archive_path, checksum_path):
            if target.exists() or target.is_symlink():
                raise PackageError("archive output already exists; refusing to replace it")
        archive_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".vellum-package-", dir=output.parent) as stage:
        stage_path = Path(stage)
        bundle = stage_path / "bundle"
        bundle.mkdir(mode=0o700)
        builds = []
        for name in BINARIES:
            binary = copy_member(binaries, name, bundle, "bin/" + name, True, MAX_BINARY_BYTES)
            before = digest_file(binary)
            builds.append(probe_build(binary, timeout))
            if digest_file(binary) != before:
                raise PackageError("binary changed during build-info probing")
        if any(build != builds[0] for build in builds[1:]):
            raise PackageError("the four binaries do not have identical build information")
        build = builds[0]
        official = verify_release_tag(source, build, tag)
        for relative, destination in RESOURCES.items():
            copy_member(source, relative, bundle, destination, destination == "install.sh",
                        MAX_RESOURCE_BYTES)
        label = "Clean tagged release candidate" if official else "DEVELOPMENT BUNDLE (not an official release)"
        readme = f"""# Vellum {build['version']}

{label}
Build ID: {build['build_id']}
Target: {build['target']}; profile: {build['profile']}; source dirty: {str(build['source_dirty']).lower()}.

This is a binary bundle, not a source/developer backup. It contains no screenshots,
user configuration, editable source images, recovery assets or compositor bindings.
Requires Arch Linux / Wayland, GTK >= 4.12 and gtk4-layer-shell. Capture/clipboard/
notifications/OCR also use grim, wl-clipboard, libnotify and Tesseract language data.
No dependency is installed automatically by this bundle.

Verify the adjacent archive .sha256 before extracting, then review manifest.json.
Checksums detect corruption; they do not authenticate a publisher. An official CI
artifact may additionally carry GitHub build provenance for the fixed source tag.

From this extracted directory: bash ./install.sh [release install options]
The entry point calls only this bundle's bin/vellum release install --bundle here.
It does not fetch source, build code, or silently adopt an unknown old installation.
Use --adopt-legacy only when you intentionally want the manager to back up and
replace a supported old installation. Default user settings and screenshots are
preserved. To inspect options: ./bin/vellum release install --help

Resources retain @VELLUM_LAUNCHER@ / @VELLUM_TRAY@ placeholders; the manager renders
per-version generated resources. This bundle does not alter niri/Hyprland bindings.
License: MIT; see LICENSE.
"""
        (bundle / "README.md").write_text(readme, encoding="utf-8")
        (bundle / "README.md").chmod(0o644)
        if official:
            verify_release_tag(source, build, tag)  # Recheck after copying mutable resource inputs.
        write_manifest(bundle, build)
        top = ("vellum-" if official else "vellum-dev-") + build["build_id"]
        # Flush the complete directory before publishing it. The destination may
        # only be absent or an empty caller-provided placeholder, never a live bundle.
        for member in sorted(bundle.rglob("*")):
            if member.is_file():
                with member.open("rb") as stream:
                    os.fsync(stream.fileno())
        for directory in sorted((p for p in bundle.rglob("*") if p.is_dir()),
                                key=lambda p: len(p.parts), reverse=True):
            sync_directory(directory)
        sync_directory(bundle)
        if output.is_symlink() or (output.exists() and (not output.is_dir() or any(output.iterdir()))):
            raise PackageError("output changed during packaging; refusing to overwrite it")
        os.rename(bundle, output)  # Atomic; a racing nonempty directory is rejected by the OS.
        try:
            sync_directory(output.parent)
        except OSError as error:
            raise PackageError("bundle committed, but directory durability was not confirmed") from error
        report = {"bundle_dir": str(output), "archive": None, "checksum": None,
                  "release_id": build["build_id"],
                  "kind": "tagged-candidate" if official else "development"}
        if archive_path is not None:
            # The archive stage lives beside its final path for same-filesystem
            # exclusive publication. A failed optional archive does not remove the bundle.
            with tempfile.TemporaryDirectory(prefix=".vellum-archive-", dir=archive_path.parent) as archive_stage:
                archive = Path(archive_stage) / "archive.tar.gz"
                archive_bundle(output, archive, top, epoch)
                checksum = digest_file(archive)
                staged_checksum = Path(archive_stage) / "checksum"
                staged_checksum.write_text(checksum + "  " + archive_path.name + "\n", encoding="ascii")
                with staged_checksum.open("rb") as stream:
                    os.fsync(stream.fileno())
                archive.chmod(0o644)
                staged_checksum.chmod(0o644)
                try:
                    os.link(archive, archive_path)
                    os.link(staged_checksum, checksum_path)
                    sync_directory(archive_path.parent)
                except OSError as error:
                    raise PackageError("bundle committed, but optional archive/checksum publication needs checking") from error
                report.update(archive=str(archive_path), checksum=str(checksum_path), sha256=checksum)
        return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent, help="source checkout containing the allowlisted release resources")
    parser.add_argument("--bin-dir", type=Path, required=True, help="directory with four prebuilt binaries")
    parser.add_argument("--output", type=Path, required=True, help="new or empty complete bundle directory")
    parser.add_argument("--archive", type=Path, help="optional .tar.gz outside the bundle; also writes .sha256")
    parser.add_argument("--release-tag", help="require this exact vVERSION tag and a clean release build")
    parser.add_argument("--source-date-epoch", type=int, default=os.environ.get("SOURCE_DATE_EPOCH", "0"))
    parser.add_argument("--probe-timeout", type=float, default=3.0)
    args = parser.parse_args()
    try:
        result = package(args.root, args.bin_dir, args.output,
                         args.release_tag, args.source_date_epoch, args.probe_timeout, args.archive)
    except (PackageError, OSError, ValueError) as error:
        # Avoid embedding raw errors from processes/parsers or secrets in paths.
        detail = str(error) if isinstance(error, PackageError) else "package filesystem/input operation failed"
        parser.exit(1, "package-release: " + detail + "\n")
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
