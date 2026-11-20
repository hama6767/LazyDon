"""Offline installer integration tests: python3 -m unittest discover -s tests."""

import hashlib
import io
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"
PLATFORMS = (
    ("Linux", "x86_64", "lazydon-linux-x86_64.tar.gz", True),
    ("Linux", "aarch64", "lazydon-linux-aarch64.tar.gz", True),
    ("Darwin", "arm64", "lazydon-macos-aarch64.tar.gz", False),
)


class InstallerTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="lazydon installer ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.mock = self.root / "mock"
        self.assets = self.root / "assets"
        self.dest = self.root / "destination with spaces"
        self.mock.mkdir()
        self.assets.mkdir()
        for name in ("awk", "tar", "gzip", "mktemp", "install", "rm", "mkdir", "mv", "shasum"):
            executable = shutil.which(name)
            if executable is None:
                self.skipTest(f"{name} is required for installer integration tests")
            (self.mock / name).symlink_to(executable)
        self.mock_command("uname", 'case "$1" in -s) printf "%s\\n" "$MOCK_OS";; -m) printf "%s\\n" "$MOCK_ARCH";; esac')
        self.mock_command("curl", 'while [ "$#" -gt 0 ]; do case "$1" in https://*) url=$1;; -o) shift; out=$1;; esac; shift; done\n/bin/cp "$MOCK_ASSETS/${url##*/}" "$out"')
        self.mock_command("sleep", ":")

    def mock_command(self, name, body):
        script = self.mock / name
        script.write_text("#!/bin/sh\n" + body + "\n")
        script.chmod(0o755)

    def prepare(self, osname, arch, archive, use_sha256sum):
        binary = b"#!/bin/sh\necho test\n"
        path = self.assets / archive
        with tarfile.open(path, "w:gz") as tar:
            info = tarfile.TarInfo("./lazydon")
            info.size = len(binary)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(binary))
        checksum = self.assets / (archive + ".sha256")
        checksum.write_text(hashlib.sha256(path.read_bytes()).hexdigest() + "  " + archive + "\n")
        sha256sum = self.mock / "sha256sum"
        sha256sum.unlink(missing_ok=True)
        if use_sha256sum:
            executable = shutil.which("sha256sum")
            if executable is None:
                self.skipTest("sha256sum is required for Linux test cases")
            sha256sum.symlink_to(executable)
        env = os.environ | {
            "PATH": str(self.mock), "HOME": str(self.root), "MOCK_OS": osname,
            "MOCK_ARCH": arch, "MOCK_ASSETS": str(self.assets),
            "LAZYDON_INSTALL_DIR": str(self.dest),
        }
        return binary, path, checksum, env

    def run_installer(self, env):
        return subprocess.run(["/bin/sh", str(INSTALLER)], env=env, capture_output=True, text=True)

    def test_platform_selection_and_checksum_failures_preserve_existing_binary(self):
        for osname, arch, archive, use_sha256sum in PLATFORMS:
            with self.subTest(os=osname, architecture=arch):
                binary, path, checksum, env = self.prepare(osname, arch, archive, use_sha256sum)
                result = self.run_installer(env)
                self.assertEqual(result.returncode, 0, result.stderr)
                target = self.dest / "lazydon"
                self.assertEqual(target.read_bytes(), binary)
                self.assertTrue(target.stat().st_mode & stat.S_IXUSR)
                target.write_bytes(b"existing binary")
                for bad_checksum in ("0" * 64 + "  " + archive + "\n", "invalid checksum\n"):
                    checksum.write_text(bad_checksum)
                    result = self.run_installer(env)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(target.read_bytes(), b"existing binary")
                self.assertFalse(list(self.dest.glob(".lazydon.*")))

    def test_missing_release_asset_preserves_existing_binary(self):
        binary, path, checksum, env = self.prepare(*PLATFORMS[0])
        self.dest.mkdir()
        target = self.dest / "lazydon"
        target.write_bytes(b"existing binary")
        path.unlink()
        result = self.run_installer(env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("try again", result.stderr)
        self.assertEqual(target.read_bytes(), b"existing binary")
        self.assertFalse(list(self.dest.glob(".lazydon.*")))


if __name__ == "__main__":
    unittest.main()
