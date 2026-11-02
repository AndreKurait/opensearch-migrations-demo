"""Offline installer regressions using local archives and a fake download command."""

import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
MEMBER = f"ma-demo-{TARGET}"
TARBALL = f"ma-demo-9.8.7-{TARGET}.tar.gz"
BINARY = b"#!/bin/sh\nprintf 'fixture binary\\n'\n"


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.destination = self.root / "destination with spaces"
        self.destination.mkdir()
        self.installed = self.destination / "ma-demo"
        self.installed.write_bytes(b"previous installation")
        for command in ["tar", "gzip", "awk", "mktemp", "rm", "mkdir", "install", "mv", "grep", "tr"]:
            (self.tools / command).symlink_to(shutil.which(command))
        self.script("uname", "import sys\nprint('Linux' if sys.argv[-1] == '-s' else 'x86_64')\n")
        self.script(
            "curl",
            """import os, pathlib, sys
args = sys.argv[1:]
url = args[-1]
if url.endswith('.sha256'):
    if os.environ.get('CHECKSUM_MISSING') == '1':
        sys.exit(22)
    sys.stdout.write(pathlib.Path(os.environ['FIXTURE_CHECKSUM']).read_text())
elif '-o' in args:
    pathlib.Path(args[args.index('-o') + 1]).write_bytes(pathlib.Path(os.environ['FIXTURE_ARCHIVE']).read_bytes())
else:
    raise SystemExit('unexpected network request: ' + url)
""",
        )
        self.script(
            "sha256sum",
            "import hashlib, pathlib, sys\nprint(hashlib.sha256(pathlib.Path(sys.argv[-1]).read_bytes()).hexdigest(), sys.argv[-1])\n",
        )
        self.archive()

    def script(self, name, body):
        filename = self.tools / name
        filename.write_text(f"#!{sys.executable}\n{body}")
        filename.chmod(0o755)

    def archive(self, names=None, symlink=False):
        self.archive_path = self.root / TARBALL
        with tarfile.open(self.archive_path, "w:gz") as archive:
            for name in names or [MEMBER]:
                member = tarfile.TarInfo(name)
                member.mode = 0o755
                if symlink:
                    member.type = tarfile.SYMTYPE
                    member.linkname = "/bin/sh"
                    archive.addfile(member)
                else:
                    member.size = len(BINARY)
                    archive.addfile(member, io.BytesIO(BINARY))
        self.checksum_path = self.root / "checksum"
        self.checksum_path.write_text(
            f"{hashlib.sha256(self.archive_path.read_bytes()).hexdigest()}  {TARBALL}\n"
        )

    def run_installer(self, **overrides):
        env = dict(
            os.environ,
            PATH=str(self.tools),
            BIN_DIR=str(self.destination),
            MA_DEMO_VERSION="v9.8.7",
            FIXTURE_ARCHIVE=str(self.archive_path),
            FIXTURE_CHECKSUM=str(self.checksum_path),
        )
        env.update(overrides)
        result = subprocess.run(
            ["/bin/bash", str(ROOT / "install.sh")],
            env=env, text=True, capture_output=True, timeout=15,
        )
        return result

    def assert_rejected(self, **overrides):
        result = self.run_installer(**overrides)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.installed.read_bytes(), b"previous installation")
        self.assertEqual(list(self.destination.iterdir()), [self.installed])

    def test_verified_archive_replaces_existing_binary(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.installed.read_bytes(), BINARY)
        self.assertEqual(self.installed.stat().st_mode & 0o777, 0o755)

    def test_missing_checksum_stops_before_install(self):
        self.assert_rejected(CHECKSUM_MISSING="1")

    def test_missing_checksum_tool_stops_before_install(self):
        (self.tools / "sha256sum").unlink()
        self.assert_rejected()

    def test_mismatched_checksum_is_rejected(self):
        self.checksum_path.write_text(f"{'0' * 64}  {TARBALL}\n")
        self.assert_rejected()

    def test_malformed_or_ambiguous_checksum_is_rejected(self):
        for checksum in ["", "not-a-checksum", f"{'a' * 64} other.tar.gz\n", "a\nb\n"]:
            with self.subTest(checksum=checksum):
                self.checksum_path.write_text(checksum)
                self.assert_rejected()

    def test_wrong_binary_name_does_not_trigger_a_fallback(self):
        self.archive(["ma-demo-wrong-platform"])
        self.assert_rejected()

    def test_extra_and_traversal_members_are_rejected(self):
        for names in [[MEMBER, "extra"], [MEMBER, "../../escape"], [MEMBER, MEMBER]]:
            with self.subTest(names=names):
                self.archive(names)
                self.assert_rejected()
        self.assertFalse((self.root.parent / "escape").exists())

    def test_symlink_binary_is_rejected(self):
        self.archive(symlink=True)
        self.assert_rejected()

    def test_failed_staged_install_preserves_existing_binary(self):
        (self.tools / "install").unlink()
        self.script("install", "raise SystemExit(43)\n")
        self.assert_rejected()


if __name__ == "__main__":
    unittest.main()
