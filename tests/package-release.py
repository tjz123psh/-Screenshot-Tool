#!/usr/bin/env python3
"""Offline package tests: temporary trees and fake binaries, never real installers."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from unittest import mock

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("vellum_package_release", ROOT / "tools/package_release.py")
package_release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package_release)
P = package_release


def build_info(dirty=True):
    return {"format": P.BUILD_FORMAT, "version": "0.2.0", "build_id": "0.2.0-test-123abc",
            "source_commit": "a" * 40, "source_dirty": dirty, "source_digest": "b" * 64,
            "target": "x86_64-unknown-linux-gnu", "rustc": "rustc 1.90.0 (test)",
            "profile": "release", "config_schema": 1, "ipc_schema": 1}


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="vellum package test ")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.source = self.root / "source tree"
        self.binaries = self.root / "prebuilt binaries"
        self.source.mkdir()
        self.binaries.mkdir()
        for relative in P.RESOURCES:
            path = self.source / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            if relative == "contrib/install-bundle.sh":
                path.write_bytes((ROOT / relative).read_bytes())
                path.chmod(0o755)
            else:
                path.write_text("fixture " + relative + "\n@VELLUM_LAUNCHER@ @VELLUM_TRAY@\n")
        self.info = build_info()
        self.make_binaries(self.info)

    def make_binary(self, name, body, marker=True):
        binary = self.binaries / name
        binary.write_text("#!" + sys.executable + "\n" +
                          ("# " + P.BUILD_FORMAT + "\n" if marker else "") + body)
        binary.chmod(0o755)
        return binary

    def make_binaries(self, info):
        for name in P.BINARIES:
            self.make_binary(name, "import sys\nassert sys.argv[1:] == ['--build-info-json']\nprint(" +
                             repr(json.dumps(info)) + ")\n")

    def package(self, output="out", **kwargs):
        prefix = "vellum-" if kwargs.get("tag") else "vellum-dev-"
        return P.package(self.source, self.binaries, self.root / output,
                         archive_path=self.root / (prefix + output + ".tar.gz"), **kwargs)

    def members(self, report):
        with tarfile.open(report["archive"], "r:gz") as archive:
            files = {member.name.split("/", 1)[1]: archive.extractfile(member).read()
                     for member in archive.getmembers() if member.isfile()}
            modes = {member.name.split("/", 1)[1]: member.mode
                     for member in archive.getmembers() if member.isfile()}
            for member in archive:
                self.assertFalse(member.issym() or member.islnk())
                self.assertEqual(member.uid, 0)
                self.assertEqual(member.gid, 0)
                self.assertEqual(member.mtime, 0)
        return files, modes

    def test_manifest_hashes_modes_templates_and_no_private_or_source_assets(self):
        for relative in ("config.toml", "Screenshots/private.png", "crates/source.rs", "contrib/niri-vellum.kdl"):
            path = self.source / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("private fixture must not ship")
        report = self.package()
        self.assertEqual(report["kind"], "development")
        self.assertIn("vellum-dev-", Path(report["archive"]).name)
        files, modes = self.members(report)
        manifest = json.loads(files["manifest.json"])
        self.assertEqual(set(manifest), {"format", "release_id", "version", "build", "files"})
        self.assertEqual(manifest["build"], self.info)
        expected = {"bin/" + name for name in P.BINARIES} | set(P.RESOURCES.values()) | {"README.md"}
        self.assertEqual(set(files), expected | {"manifest.json"})
        self.assertEqual({entry["path"] for entry in manifest["files"]}, expected)
        for entry in manifest["files"]:
            self.assertEqual(set(entry), {"path", "sha256", "size", "executable"})
            payload = files[entry["path"]]
            self.assertEqual(entry["sha256"], hashlib.sha256(payload).hexdigest())
            self.assertEqual(entry["size"], len(payload))
            self.assertEqual(modes[entry["path"]], 0o755 if entry["executable"] else 0o644)
        self.assertIn(b"@VELLUM_LAUNCHER@", files["resources/applications/ai.vellum.desktop"])
        self.assertIn(b"DEVELOPMENT BUNDLE", files["README.md"])
        self.assertNotIn(b"private fixture", b"".join(files.values()))
        checksum = Path(report["checksum"]).read_text().split()[0]
        self.assertEqual(checksum, hashlib.sha256(Path(report["archive"]).read_bytes()).hexdigest())

    def test_same_inputs_produce_identical_tar_gzip_despite_source_mtimes(self):
        first = self.package("first")
        for root in (self.source, self.binaries):
            for member in root.rglob("*"):
                os.utime(member, (1234567890, 1234567890))
        second = self.package("second")
        self.assertEqual(Path(first["archive"]).read_bytes(), Path(second["archive"]).read_bytes())

    def test_missing_marker_refuses_old_tray_without_executing_it(self):
        sentinel = self.root / "old-tray-started"
        binary = self.make_binary("vellum-tray", "from pathlib import Path\nPath(" + repr(str(sentinel)) + ").write_text('bad')\n", marker=False)
        with self.assertRaisesRegex(P.PackageError, "marker"):
            P.probe_build(binary, 0.2)
        self.assertFalse(sentinel.exists())

    def test_metadata_timeout_and_output_budgets(self):
        sleeper = self.make_binary("vellum", "import time\ntime.sleep(60)\n")
        start = time.monotonic()
        with self.assertRaisesRegex(P.PackageError, "timed out"):
            P.probe_build(sleeper, 0.1)
        self.assertLess(time.monotonic() - start, 2)
        for stream in ("stdout", "stderr"):
            flood = self.make_binary("vellum", "import sys\nsys." + stream + ".write('x' * 100000)\n")
            with self.assertRaisesRegex(P.PackageError, "limit"):
                P.probe_build(flood, 1)

    def test_probe_has_no_inherited_user_configuration_or_desktop_environment(self):
        body = "import os\nassert 'DISPLAY' not in os.environ\nassert 'VELLUM_TEST_SECRET' not in os.environ\nassert os.environ['HOME'] == os.environ['XDG_CONFIG_HOME']\nprint(" + repr(json.dumps(self.info)) + ")\n"
        binary = self.make_binary("vellum", body)
        with mock.patch.dict(os.environ, {"DISPLAY": ":999", "VELLUM_TEST_SECRET": "not-a-real-secret"}):
            self.assertEqual(P.probe_build(binary, 1), self.info)

    def test_mismatched_binary_identity_and_unsafe_ids_are_rejected(self):
        mismatch = dict(self.info, source_digest="c" * 64)
        self.make_binary("vellum-tray", "print(" + repr(json.dumps(mismatch)) + ")\n")
        with self.assertRaisesRegex(P.PackageError, "identical"):
            self.package()
        for value in ("../bad", "x/y", "x\\y", "..", "x" * 97, "x\nname"):
            with self.assertRaises(P.PackageError):
                P.validate_build(dict(self.info, build_id=value))
        for value in (1, "false", None):
            with self.assertRaises(P.PackageError):
                P.validate_build(dict(self.info, source_dirty=value))
        with self.assertRaises(P.PackageError):
            P.validate_build(dict(self.info, unexpected="value"))

    def test_invalid_duplicate_json_and_child_failure_do_not_echo_output(self):
        for body in ("print('secret-private-endpoint')\n", "raise SystemExit(4)\n", "print('{\"format\": 1, \"format\": 2}')\n"):
            binary = self.make_binary("vellum", body)
            with self.assertRaises(P.PackageError) as caught:
                P.probe_build(binary, 1)
            self.assertNotIn("secret-private-endpoint", str(caught.exception))

    def test_symlink_and_nonregular_inputs_are_rejected(self):
        icon = self.source / "contrib/icons/ai.vellum.svg"
        real = self.root / "external-icon"
        icon.rename(real)
        icon.symlink_to(real)
        with self.assertRaisesRegex(P.PackageError, "symlink"):
            self.package()
        with self.assertRaises(P.PackageError):
            P.checked_file(self.source, "../external-icon", False, 1024)
        with self.assertRaises(P.PackageError):
            P.checked_file(self.source, "contrib", False, 1024)
        with self.assertRaises(P.PackageError):
            P.checked_file(self.binaries, "vellum", True, 1)

    def test_existing_output_is_never_replaced(self):
        first = self.package()
        original = Path(first["archive"]).read_bytes()
        with self.assertRaisesRegex(P.PackageError, "existing content"):
            self.package()
        self.assertEqual(Path(first["archive"]).read_bytes(), original)
        self.assertFalse(list(self.root.glob(".vellum-package-*")))

    def test_formal_candidates_require_exact_clean_tag_and_known_source(self):
        with self.assertRaises(P.PackageError):
            self.package(tag="v0.2.0")
        info = build_info(dirty=False)
        self.make_binaries(info)
        def git(_source, *args):
            return "" if args[0] == "status" else info["source_commit"]
        with mock.patch.object(P, "git_output", side_effect=git):
            report = self.package(tag="v0.2.0")
            self.assertEqual(report["kind"], "tagged-candidate")
            self.assertNotIn("vellum-dev-", Path(report["archive"]).name)
            self.assertIn(b"Clean tagged release candidate", self.members(report)[0]["README.md"])
            with self.assertRaises(P.PackageError):
                self.package("bad-tag", tag="main")
        with mock.patch.object(P, "git_output", return_value="mismatch"):
            with self.assertRaises(P.PackageError):
                self.package("bad-head", tag="v0.2.0")
        with self.assertRaises(P.PackageError):
            P.verify_release_tag(self.source, dict(info, source_commit="unknown"), "v0.2.0")

    def test_real_temporary_git_tag_matches_build_and_dirty_checkout_is_rejected(self):
        empty_template = self.root / "empty git template"
        empty_template.mkdir()
        env = {"PATH": "/usr/bin:/bin", "HOME": str(self.root), "GIT_CONFIG_NOSYSTEM": "1",
               "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_TEMPLATE_DIR": str(empty_template)}
        def git(*args):
            return subprocess.run(["git", "-C", str(self.source), *args], check=True,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  env=env, timeout=5).stdout.decode().strip()
        git("init", "--quiet")
        git("add", ".")
        git("-c", "user.name=Package Test Fixture", "-c", "user.email=fixture@example.invalid",
            "commit", "--quiet", "-m", "fixture")
        commit = git("rev-parse", "HEAD")
        git("tag", "v0.2.0")
        self.make_binaries(dict(build_info(dirty=False), source_commit=commit))
        with mock.patch.dict(os.environ, env, clear=True):
            report = self.package(tag="v0.2.0")
            self.assertEqual(report["kind"], "tagged-candidate")
            (self.source / "unexpected.txt").write_text("not part of tagged source")
            with self.assertRaisesRegex(P.PackageError, "uncommitted"):
                self.package("dirty-source", tag="v0.2.0")

    def test_directory_only_cli_accepts_empty_output_and_never_archives_or_installs(self):
        output = self.root / "directory bundle"
        output.mkdir()
        result = subprocess.run([sys.executable, str(ROOT / "tools/package_release.py"),
                                 "--root", str(self.source), "--bin-dir", str(self.binaries),
                                 "--output", str(output)], stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, check=True, timeout=5)
        report = json.loads(result.stdout)
        self.assertEqual(report["bundle_dir"], str(output))
        self.assertIsNone(report["archive"])
        self.assertTrue((output / "manifest.json").is_file())
        self.assertFalse(list(self.root.glob("*.tar.gz")))
        marker = output / "unknown-user-file"
        marker.write_text("keep")
        with self.assertRaises(P.PackageError):
            P.package(self.source, self.binaries, output)
        self.assertEqual(marker.read_text(), "keep")

    def test_unknown_source_is_explicitly_development_only(self):
        unknown = dict(self.info, source_dirty="unknown", source_commit="unknown", source_digest="unknown")
        self.make_binaries(unknown)
        report = self.package()
        self.assertEqual(report["kind"], "development")
        self.assertEqual(json.loads(self.members(report)[0]["manifest.json"])["build"]["source_dirty"], "unknown")
        with self.assertRaises(P.PackageError):
            self.package("not-official", tag="v0.2.0")

    def test_manual_workflow_shell_syntax_and_tag_provenance_contract(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("workflow_dispatch:", workflow)
        self.assertNotIn("  push:", workflow)
        self.assertNotIn("contents: write", workflow)
        self.assertIn('test "$WORKFLOW_REF" = "refs/tags/$RELEASE_TAG"', workflow)
        self.assertIn('test "$(git rev-parse HEAD)" = "$WORKFLOW_SHA"', workflow)
        self.assertIn("actions/attest-build-provenance@", workflow)
        self.assertIn('--output "$OUTPUT_DIR/bundle"', workflow)
        lines = workflow.splitlines()
        for index, line in enumerate(lines):
            if line.strip() != "run: |":
                continue
            script = []
            for following in lines[index + 1:]:
                if following and not following.startswith("          "):
                    break
                script.append(following[10:] if following else "")
            subprocess.run(["bash", "-n"], input="\n".join(script).encode(), check=True, timeout=3)

    def test_bundle_entry_only_forwards_to_its_own_fake_manager(self):
        bundle = self.root / "fake extracted bundle"
        (bundle / "bin").mkdir(parents=True)
        installer = bundle / "install.sh"
        installer.write_bytes((ROOT / "contrib/install-bundle.sh").read_bytes())
        log = self.root / "manager-arguments.json"
        binary = bundle / "bin/vellum"
        binary.write_text("#!" + sys.executable + "\nimport json,sys\nfrom pathlib import Path\nPath(" + repr(str(log)) + ").write_text(json.dumps(sys.argv[1:]))\n")
        binary.chmod(0o755)
        subprocess.run(["bash", str(installer), "--root", "path with spaces", "--json"],
                       cwd=self.root, check=True, timeout=3)
        self.assertEqual(json.loads(log.read_text()), ["release", "install", "--bundle", str(bundle),
                                                       "--root", "path with spaces", "--json"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
