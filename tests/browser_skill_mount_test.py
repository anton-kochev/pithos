"""Offline filesystem tests; never modify the real Pi home."""
import pathlib
import runpy
import tempfile
import unittest

prepare = runpy.run_path(str(pathlib.Path(__file__).parents[1] / 'src/browser/prepare_skill.py'))['prepare']


class SkillMountTests(unittest.TestCase):
    def test_reused_mount_stays_empty_without_skill_bytes(self):
        with tempfile.TemporaryDirectory() as root:
            prepare(root)
            prepare(root)
            target = pathlib.Path(root) / '.agents/skills/pithos-browser'
            self.assertEqual(list(target.iterdir()), [])

    def test_collision_is_preserved_and_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            prepare(root)
            target = pathlib.Path(root) / '.agents/skills/pithos-browser/SKILL.md'
            target.write_text('user-owned skill')
            with self.assertRaises(RuntimeError):
                prepare(root)
            self.assertEqual(target.read_text(), 'user-owned skill')

    def test_symlink_ancestor_is_not_followed(self):
        with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as other:
            (pathlib.Path(root) / '.agents').symlink_to(other, target_is_directory=True)
            with self.assertRaises(OSError):
                prepare(root)
            self.assertEqual(list(pathlib.Path(other).iterdir()), [])


if __name__ == '__main__':
    unittest.main()
