"""Run the real startup script with container paths redirected to isolated fixtures."""
import os
import pathlib
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
NAME = "pithos-instructions.ts"


class InstructionsStartupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.agent = self.root / "agent"
        self.defaults = self.root / "defaults"
        (self.defaults / "extensions").mkdir(parents=True)
        self.bundled = self.defaults / "extensions" / NAME
        self.bundled.write_text("current bundled instructions\n")
        self.target = self.agent / "extensions" / NAME
        script = (ROOT / "entrypoint.sh").read_text()
        for old, new in [("/home/pi/.pi/agent", self.agent),
                         ("/opt/pi-defaults", self.defaults),
                         ("/etc/pithos/extensions.list", self.root / "absent-manifest")]:
            script = script.replace(old, str(new))
        self.script = self.root / "entrypoint.sh"
        self.script.write_text(script)

    def start(self):
        return subprocess.run(
            ["bash", str(self.script), "bash", "-c", "printf startup-complete"],
            env={**os.environ, "GIT_USER_NAME": "", "GIT_USER_EMAIL": ""},
            capture_output=True, text=True)

    def assert_started(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "startup-complete")

    def test_stale_instructions_refresh_without_clobbering_user_files(self):
        self.target.parent.mkdir(parents=True)
        self.target.write_text("stale instructions\n")
        user = self.target.parent / "user.ts"
        user.write_text("user extension\n")
        config = self.agent / "custom.json"
        config.write_text('{"user": true}\n')
        (self.defaults / "extensions" / "user.ts").write_text("default extension\n")
        (self.defaults / "custom.json").write_text("{}\n")
        self.assert_started(self.start())
        self.assertEqual(self.target.read_text(), self.bundled.read_text())
        self.assertEqual(user.read_text(), "user extension\n")
        self.assertEqual(config.read_text(), '{"user": true}\n')

    def test_fresh_and_repeat_startup_use_current_bundle(self):
        self.assert_started(self.start())
        self.assertEqual(self.target.read_text(), self.bundled.read_text())
        self.assert_started(self.start())
        self.bundled.write_text("next image instructions\n")
        self.assert_started(self.start())
        self.assertEqual(self.target.read_text(), "next image instructions\n")
        self.assertEqual(list(self.target.parent.iterdir()), [self.target])

    def test_file_symlinks_are_replaced_without_writing_their_targets(self):
        for kind in ["file", "directory", "missing"]:
            with self.subTest(kind=kind):
                outside = self.root / kind
                if kind == "file":
                    outside.write_text("user content\n")
                elif kind == "directory":
                    outside.mkdir()
                self.target.parent.mkdir(parents=True, exist_ok=True)
                self.target.unlink(missing_ok=True)
                self.target.symlink_to(outside, target_is_directory=kind == "directory")
                self.assert_started(self.start())
                self.assertFalse(self.target.is_symlink())
                self.assertEqual(self.target.read_text(), self.bundled.read_text())
                if kind == "file":
                    self.assertEqual(outside.read_text(), "user content\n")
                elif kind == "directory":
                    self.assertEqual(list(outside.iterdir()), [])
                else:
                    self.assertFalse(outside.exists())
                self.target.unlink()

    def test_missing_bundle_fails_visibly_and_preserves_stale_file(self):
        self.target.parent.mkdir(parents=True)
        self.target.write_text("stale instructions\n")
        self.bundled.unlink()
        result = self.start()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed to refresh managed pithos-instructions.ts", result.stderr)
        self.assertNotIn("startup-complete", result.stdout)
        self.assertEqual(self.target.read_text(), "stale instructions\n")
        self.assertEqual(list(self.target.parent.iterdir()), [self.target])

    def test_destination_directory_is_not_used_as_a_copy_target(self):
        self.target.mkdir(parents=True)
        result = self.start()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed to refresh managed pithos-instructions.ts", result.stderr)
        self.assertNotIn("startup-complete", result.stdout)
        self.assertEqual(list(self.target.iterdir()), [])
        self.assertEqual(list(self.target.parent.iterdir()), [self.target])

    def test_symlinked_extensions_directory_fails_without_touching_user_directory(self):
        outside = self.root / "user-extensions"
        outside.mkdir()
        sentinel = outside / NAME
        sentinel.write_text("user-owned instructions\n")
        self.agent.mkdir()
        self.target.parent.symlink_to(outside, target_is_directory=True)
        result = self.start()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pithos-instructions.ts", result.stderr)
        self.assertNotIn("startup-complete", result.stdout)
        self.assertEqual(sentinel.read_text(), "user-owned instructions\n")
        self.assertEqual(list(outside.iterdir()), [sentinel])


if __name__ == "__main__":
    unittest.main()
