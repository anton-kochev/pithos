"""Offline tests for the exact Python helper embedded in the Rust launcher.
Run: python3 -m unittest discover -s tests -p '*_test.py'
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'src/sessions/migrate.py'


class MigrationTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.source = Path(self.temp.name) / 'legacy/.pi/agent/sessions'
        self.target = Path(self.temp.name) / 'destination'
        self.source.mkdir(parents=True)
        self.target.mkdir()
        (self.target / '.gitignore').write_text('*\n!.gitignore\n')

    def run_import(self):
        return subprocess.run(['python3', str(SCRIPT), str(self.source), str(self.target)], capture_output=True, text=True)

    def test_preserves_tree_timestamps_source_and_no_clobber(self):
        folder = self.source / '--workspace-project--'
        folder.mkdir()
        transcript = folder / 'one.jsonl'
        transcript.write_text('original')
        os.utime(transcript, ns=(1000000000, 2000000000))
        (self.source.parent / 'auth.json').write_text('credential')
        first = self.run_import()
        self.assertEqual(first.returncode, 0, first.stderr)
        target = self.target / folder.name / transcript.name
        self.assertEqual(target.read_text(), 'original')
        self.assertEqual(target.stat().st_mtime_ns, transcript.stat().st_mtime_ns)
        self.assertFalse((self.target / 'auth.json').exists())
        target.write_text('host version')
        second = self.run_import()
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn('skipped (already present): 1', second.stdout)
        self.assertEqual(target.read_text(), 'host version')
        self.assertEqual(transcript.read_text(), 'original')

    def test_rejects_source_symlink(self):
        (self.source / 'bad').symlink_to('/etc/passwd')
        self.assertNotEqual(self.run_import().returncode, 0)
        self.assertFalse((self.target / 'bad').exists())

    def test_rejects_destination_symlink(self):
        (self.source / 'folder').mkdir()
        (self.source / 'folder/data').write_text('private')
        outside = Path(self.temp.name) / 'outside'
        outside.mkdir()
        (self.target / 'folder').symlink_to(outside, target_is_directory=True)
        self.assertNotEqual(self.run_import().returncode, 0)
        self.assertEqual(list(outside.iterdir()), [])

    def test_missing_source_is_error_not_empty_success(self):
        self.source.rmdir()
        self.assertNotEqual(self.run_import().returncode, 0)

    def test_does_not_replace_git_safeguard(self):
        (self.source / '.gitignore').write_text('!*.jsonl')
        result = self.run_import()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.target / '.gitignore').read_text(), '*\n!.gitignore\n')
