"""Reject incomplete or corrupt release sets before making them public."""

import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("verify_release", ROOT / "scripts/verify_release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        (self.directory / "install.sh").write_text("#!/bin/sh\n")
        for target in release.TARGETS:
            self.archive(target)

    def archive(self, target, member_name=None, symlink=False):
        archive = self.directory / f"ma-demo-1.2.3-{target}.tar.gz"
        with tarfile.open(archive, "w:gz") as stream:
            member = tarfile.TarInfo(member_name or f"ma-demo-{target}")
            member.mode = 0o755
            if symlink:
                member.type = tarfile.SYMTYPE
                member.linkname = "/bin/sh"
                stream.addfile(member)
            else:
                member.size = len(b"fixture")
                stream.addfile(member, io.BytesIO(b"fixture"))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        archive.with_name(archive.name + ".sha256").write_text(f"{digest}  {archive.name}\n")
        return archive

    def test_complete_release_passes(self):
        release.verify(self.directory, "1.2.3")

    def test_missing_platform_or_checksum_fails(self):
        for suffix in [".tar.gz", ".tar.gz.sha256"]:
            path = self.directory / f"ma-demo-1.2.3-{release.TARGETS[0]}{suffix}"
            contents = path.read_bytes()
            path.unlink()
            with self.assertRaises(ValueError):
                release.verify(self.directory, "1.2.3")
            path.write_bytes(contents)

    def test_corrupt_archive_fails(self):
        path = self.directory / f"ma-demo-1.2.3-{release.TARGETS[0]}.tar.gz"
        path.write_bytes(path.read_bytes() + b"corruption")
        with self.assertRaisesRegex(ValueError, "checksum"):
            release.verify(self.directory, "1.2.3")

    def test_wrong_or_symlink_member_fails(self):
        for options in [{"member_name": "../../escape"}, {"symlink": True}]:
            self.archive(release.TARGETS[0], **options)
            with self.assertRaisesRegex(ValueError, "binary member"):
                release.verify(self.directory, "1.2.3")

    def test_extra_asset_and_symlink_asset_fail(self):
        extra = self.directory / "unexpected"
        extra.write_text("extra")
        with self.assertRaises(ValueError):
            release.verify(self.directory, "1.2.3")
        extra.unlink()
        installer = self.directory / "install.sh"
        installer.unlink()
        installer.symlink_to("/bin/sh")
        with self.assertRaisesRegex(ValueError, "regular file"):
            release.verify(self.directory, "1.2.3")


if __name__ == "__main__":
    unittest.main()
