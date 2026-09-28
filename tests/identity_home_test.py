"""Offline tests for the exact strict read-only home inspector."""
import importlib.util
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest import mock

# The inspector refuses symlinked ancestors; macOS temp lives under /var -> private/var.
tempfile.tempdir = os.path.realpath(tempfile.gettempdir())
SCRIPT = Path(__file__).resolve().parents[1] / 'src/docker/admit_home.py'
spec = importlib.util.spec_from_file_location('admit_home', SCRIPT)
home = importlib.util.module_from_spec(spec)
spec.loader.exec_module(home)


class HomeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'pi'
        self.root.mkdir(mode=0o700)
        self.uid, self.gid = os.geteuid(), os.getegid()
        if self.uid == 0 or self.gid == 0:
            self.skipTest('requires a non-root UID and GID; never changes ownership')

    def snapshot(self):
        result = {}
        for path in [self.root, *self.root.rglob('*')]:
            info = path.lstat()
            data = (os.readlink(path) if stat.S_ISLNK(info.st_mode)
                    else path.read_bytes() if stat.S_ISREG(info.st_mode) else None)
            result[str(path.relative_to(self.root))] = (
                info.st_mode, info.st_uid, info.st_gid, info.st_ino,
                info.st_mtime_ns, info.st_ctime_ns, data)
        return result

    def assert_rejected_unchanged(self, **kwargs):
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, '^home requires explicit migration$'):
            home.validate_home(self.root, self.uid, self.gid, **kwargs)
        self.assertEqual(self.snapshot(), before)

    def test_missing_root_is_rejected_without_provisioning(self):
        self.root.rmdir()
        with self.assertRaisesRegex(ValueError, '^home requires explicit migration$'):
            home.validate_home(self.root, self.uid, self.gid)
        self.assertFalse(self.root.exists())

    def test_foreign_root_identity_is_rejected_without_mutation(self):
        before = self.snapshot()
        for uid, gid in [(self.uid + 1, self.gid), (self.uid, self.gid + 1)]:
            with self.subTest(uid=uid, gid=gid), self.assertRaises(ValueError):
                home.validate_home(self.root, uid, gid)
        self.assertEqual(self.snapshot(), before)

    def test_late_foreign_or_root_owned_inode_is_rejected_without_mutation(self):
        (self.root / '.pi/agent/sessions').mkdir(parents=True)
        (self.root / '.pi/agent/auth.json').write_text('secret-canary')
        late = self.root / '.pi/agent/sessions/late'
        late.write_text('preserve-me')
        before = self.snapshot()
        real_stat = os.stat
        for field, value in [('st_uid', 0), ('st_gid', 0),
                             ('st_uid', self.uid + 1), ('st_gid', self.gid + 1)]:
            seen = []

            def foreign(path, *args, **kwargs):
                info = real_stat(path, *args, **kwargs)
                if os.fspath(path) == 'late':
                    seen.append(path)
                    values = {key: getattr(info, key) for key in dir(info) if key.startswith('st_')}
                    values[field] = value
                    return SimpleNamespace(**values)
                return info

            with self.subTest(field=field, value=value):
                with mock.patch.object(home.os, 'stat', side_effect=foreign):
                    with self.assertRaises(ValueError):
                        home.validate_home(self.root, self.uid, self.gid)
                self.assertTrue(seen)
                self.assertEqual(self.snapshot(), before)

    def test_structural_paths_must_be_real_directories(self):
        paths = ['.pi', '.pi/agent', '.pi/agent/sessions',
                 '.agents', '.agents/skills', '.agents/skills/pithos-browser']
        outside = Path(self.temp.name) / 'outside'
        outside.mkdir()
        for name in paths:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            for kind in ['file', 'symlink']:
                with self.subTest(name=name, kind=kind):
                    if kind == 'file':
                        path.write_text('keep-private')
                    else:
                        path.symlink_to(outside, target_is_directory=True)
                    try:
                        self.assert_rejected_unchanged(browser=True)
                    finally:
                        path.unlink()
            path.mkdir()

    def test_missing_structural_paths_stay_absent_and_symlinks_are_not_followed(self):
        outside = Path(self.temp.name) / 'outside'
        outside.mkdir()
        os.mkfifo(outside / 'must-not-scan')
        (self.root / 'external').symlink_to(outside, target_is_directory=True)
        (self.root / 'dangling').symlink_to(outside / 'missing')
        (self.root / 'loop').symlink_to(self.root, target_is_directory=True)
        for browser in [False, True]:
            before = self.snapshot()
            home.validate_home(self.root, self.uid, self.gid, browser=browser)
            self.assertEqual(self.snapshot(), before)
            self.assertFalse((self.root / '.pi').exists())
            self.assertFalse((self.root / '.agents').exists())

    def test_structural_directories_need_owner_rwx(self):
        for name in ['', '.pi', '.pi/agent', '.pi/agent/sessions',
                     '.agents', '.agents/skills', '.agents/skills/pithos-browser']:
            path = self.root / name
            path.mkdir(parents=True, exist_ok=True)
            with self.subTest(name=name):
                path.chmod(0o577)  # Other users' write bits cannot substitute for owner write.
                try:
                    self.assert_rejected_unchanged(browser=True)
                finally:
                    path.chmod(0o700)

    def test_scanned_directories_need_owner_read_and_search(self):
        path = self.root / 'ordinary'
        path.mkdir()
        for mode in [0o477, 0o377]:
            with self.subTest(mode=mode):
                path.chmod(mode)
                try:
                    with self.assertRaises(ValueError):
                        home.validate_home(self.root, self.uid, self.gid)
                finally:
                    path.chmod(0o700)
        path.chmod(0o500)
        home.validate_home(self.root, self.uid, self.gid)

    def test_browser_target_must_be_empty_only_when_enabled(self):
        target = self.root / '.agents/skills/pithos-browser'
        target.mkdir(parents=True)
        before = self.snapshot()
        home.validate_home(self.root, self.uid, self.gid, browser=True)
        self.assertEqual(self.snapshot(), before)
        for name, kind in [('auth.json', 'file'), ('.hidden', 'dir'), ('link', 'symlink')]:
            path = target / name
            if kind == 'file':
                path.write_text('credential-canary')
            elif kind == 'dir':
                path.mkdir()
            else:
                path.symlink_to('/nonexistent/private')
            try:
                with self.subTest(kind=kind):
                    home.validate_home(self.root, self.uid, self.gid, browser=False)
                    self.assert_rejected_unchanged(browser=True)
            finally:
                path.rmdir() if kind == 'dir' else path.unlink()

    def test_special_files_and_hardlinks_are_rejected_without_mutation(self):
        late = self.root / 'nested/late'
        late.parent.mkdir()
        secret = self.root / 'auth.json'
        secret.write_text('canary-do-not-read')
        for kind in ['fifo', 'hardlink', 'symlink-hardlink']:
            with self.subTest(kind=kind):
                if kind == 'fifo':
                    os.mkfifo(late)
                elif kind == 'hardlink':
                    os.link(secret, late)
                else:
                    source = self.root / 'link'
                    source.symlink_to('/private/elsewhere')
                    os.link(source, late, follow_symlinks=False)
                try:
                    self.assert_rejected_unchanged()
                finally:
                    late.unlink()

    def test_entry_budget_is_global_and_boundary_is_inclusive(self):
        (self.root / 'first').write_text('private')
        (self.root / 'nested').mkdir()
        (self.root / 'nested/last').write_text('also-private')
        self.assert_rejected_unchanged(max_entries=2)
        home.validate_home(self.root, self.uid, self.gid, max_entries=3)

    def test_depth_budget_includes_files_and_default_stops_at_64(self):
        path = self.root
        for _ in range(64):
            path = path / 'd'
            path.mkdir()
        home.validate_home(self.root, self.uid, self.gid)
        (path / 'late').write_text('keep')
        self.assert_rejected_unchanged()
        self.assert_rejected_unchanged(max_depth=2)

    def test_directory_listing_is_streamed_not_materialized(self):
        (self.root / 'file').touch()
        real_scandir = os.scandir
        yielded = []

        class Listing:
            def __init__(self, fd):
                self.inner = real_scandir(fd)

            def __enter__(self):
                first = next(self.inner)

                def stream():
                    for _ in range(3):
                        yielded.append(first.name)
                        yield first
                    raise AssertionError('listing consumed beyond entry budget')
                return stream()

            def __exit__(self, *args):
                self.inner.close()

        before = self.snapshot()
        with mock.patch.object(home.os, 'scandir', Listing):
            with self.assertRaises(ValueError):
                home.validate_home(self.root, self.uid, self.gid, max_entries=2)
        self.assertLessEqual(len(yielded), 3)
        self.assertEqual(self.snapshot(), before)

    def test_elapsed_budget_fails_closed(self):
        (self.root / 'private').write_text('not-read')
        before = self.snapshot()
        with mock.patch.object(home.time, 'monotonic', side_effect=[0, 31] + [31] * 20):
            with self.assertRaises(ValueError):
                home.validate_home(self.root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), before)

    def test_limits_cannot_disable_hard_caps(self):
        for key, values in [('max_entries', [-1, 100001, True, '2']),
                            ('max_depth', [-1, 65, True, '2']),
                            ('max_seconds', [0, -1, 31, float('inf'), float('nan'), True, '2'])]:
            for value in values:
                with self.subTest(key=key, value=value):
                    self.assert_rejected_unchanged(**{key: value})

    def test_root_and_root_ancestors_cannot_be_symlinks(self):
        alias = Path(self.temp.name) / 'alias'
        alias.symlink_to(self.root, target_is_directory=True)
        (self.root / 'nested').mkdir()
        before = self.snapshot()
        for root in [alias, str(alias) + '/', alias / 'nested']:
            with self.subTest(root=root), self.assertRaises(ValueError):
                home.validate_home(root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), before)

    def test_directory_replacement_between_stat_and_open_fails_closed(self):
        nested = self.root / 'nested'
        nested.mkdir()
        (nested / 'private').write_text('original-canary')
        real_open = os.open
        after_racer = []

        def replace(path, flags, *args, **kwargs):
            if path == 'nested' and not after_racer:
                nested.rename(Path(self.temp.name) / 'saved')
                nested.mkdir()
                after_racer.append(self.snapshot())
            return real_open(path, flags, *args, **kwargs)

        with mock.patch.object(home.os, 'open', side_effect=replace):
            with self.assertRaises(ValueError):
                home.validate_home(self.root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), after_racer[0])
        self.assertEqual((Path(self.temp.name) / 'saved/private').read_text(), 'original-canary')

    def test_leaf_change_after_metadata_read_is_detected_without_helper_writes(self):
        (self.root / '.pi/agent/sessions').mkdir(parents=True)
        (self.root / '.pi/agent/auth.json').write_text('untouched-credential')
        late = self.root / '.pi/agent/sessions/late'
        real_stat = os.stat
        for change in ['content', 'unlink', 'replace-symlink']:
            late.write_text('before-racer')
            before = self.snapshot()
            after_racer = []

            def race(path, *args, **kwargs):
                info = real_stat(path, *args, **kwargs)
                if path == 'late' and not after_racer:
                    # Actual filesystem mutations by a simulated concurrent writer,
                    # not injected metadata. Snapshot after its known write.
                    if change == 'content':
                        late.write_text('changed-by-racer')
                    else:
                        late.unlink()
                        if change == 'replace-symlink':
                            late.symlink_to('/private/outside')
                    after_racer.append(self.snapshot())
                return info

            try:
                with self.subTest(change=change):
                    with mock.patch.object(home.os, 'stat', side_effect=race):
                        with self.assertRaises(ValueError):
                            home.validate_home(self.root, self.uid, self.gid)
                    self.assertEqual(self.snapshot(), after_racer[0])
                    self.assertEqual(self.snapshot()['.pi/agent/auth.json'],
                                     before['.pi/agent/auth.json'])
            finally:
                late.unlink(missing_ok=True)

    def test_second_pass_detects_earlier_leaf_changed_in_later_subtree(self):
        earlier = self.root / 'first'
        earlier.write_text('before-racer')
        later = self.root / 'later'
        later.mkdir()
        real_scandir, real_open = os.scandir, os.open
        after_racer = []

        class OrderedListing:
            def __init__(inner, fd):
                inner.entries = real_scandir(fd)

            def __enter__(inner):
                # Only this tiny fixture is sorted; production must stream.
                return iter(sorted(inner.entries, key=lambda entry: entry.name))

            def __exit__(inner, *args):
                inner.entries.close()

        def race(path, flags, *args, **kwargs):
            if path == 'later' and not after_racer:
                earlier.write_text('changed-by-racer')
                with mock.patch.object(home.os, 'scandir', real_scandir):
                    after_racer.append(self.snapshot())
            return real_open(path, flags, *args, **kwargs)

        with mock.patch.object(home.os, 'scandir', OrderedListing), \
                mock.patch.object(home.os, 'open', side_effect=race):
            with self.assertRaises(ValueError):
                home.validate_home(self.root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), after_racer[0])

    def test_scan_error_closes_all_descriptors_without_mutation(self):
        nested = self.root / 'nested'
        nested.mkdir()
        os.mkfifo(nested / 'late')
        before = self.snapshot()
        real_open = os.open
        opened = []

        def record(*args, **kwargs):
            fd = real_open(*args, **kwargs)
            opened.append(fd)
            return fd

        with mock.patch.object(home.os, 'open', side_effect=record):
            with self.assertRaises(ValueError):
                home.validate_home(self.root, self.uid, self.gid)
        self.assertTrue(opened)
        for fd in opened:
            with self.assertRaises(OSError):
                os.fstat(fd)
        self.assertEqual(self.snapshot(), before)

    def test_foreign_symlink_inode_is_rejected_without_following_it(self):
        link = self.root / 'link'
        link.symlink_to('/not-scanned')
        before = self.snapshot()
        real_stat = os.stat

        def foreign(path, *args, **kwargs):
            info = real_stat(path, *args, **kwargs)
            if path == 'link':
                self.assertFalse(kwargs['follow_symlinks'])
                values = {key: getattr(info, key) for key in dir(info) if key.startswith('st_')}
                values['st_uid'] = 0
                return SimpleNamespace(**values)
            return info

        with mock.patch.object(home.os, 'stat', side_effect=foreign):
            with self.assertRaises(ValueError):
                home.validate_home(self.root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), before)

    def test_directory_change_at_end_of_listing_fails_closed(self):
        real_scandir = os.scandir
        after_racer = []

        class Listing:
            def __init__(inner, fd):
                inner.entries = real_scandir(fd)

            def __enter__(inner):
                return inner.entries

            def __exit__(inner, *args):
                inner.entries.close()
                if not after_racer:
                    (self.root / 'late').write_text('created-by-racer')
                    # Avoid the patched listing while recording the known write.
                    with mock.patch.object(home.os, 'scandir', real_scandir):
                        after_racer.append(self.snapshot())

        with mock.patch.object(home.os, 'scandir', Listing):
            with self.assertRaises(ValueError):
                home.validate_home(self.root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), after_racer[0])

    def invoke_cli(self, args):
        return subprocess.run([sys.executable, '-c', SCRIPT.read_text(), *args],
                              capture_output=True, text=True, timeout=5)

    def test_cli_reports_static_inspection_success(self):
        ids = [str(self.uid), str(self.gid)]
        before = self.snapshot()
        for suffix in [[], ['--browser']]:
            result = self.invoke_cli([str(self.root), *ids, *suffix])
            self.assertEqual((result.returncode, result.stdout, result.stderr),
                             (0, 'home inspection passed\n', ''))
        self.assertEqual(self.snapshot(), before)

    def test_cli_failures_are_static_without_tracebacks_or_inputs(self):
        ids = [str(self.uid), str(self.gid)]
        invalid = [[], [str(self.root)], [str(self.root), *ids, '--secret-token'],
                   [str(self.root / 'secret-path-canary'), *ids],
                   [str(self.root), '0', ids[1]]]
        for value in ['credential-canary', '-1', '4294967294', '1' * 5000,
                      '+501', '0501', ' 501', '５01']:
            invalid.append([str(self.root), value, ids[1]])
        for args in invalid:
            with self.subTest(args=args[:1]):
                result = self.invoke_cli(args)
                self.assertEqual((result.returncode, result.stdout, result.stderr),
                                 (1, '', 'home requires explicit migration\n'))
        (self.root / '.pi').write_text('credential-canary-never-echo')
        before = self.snapshot()
        result = self.invoke_cli([str(self.root), *ids])
        self.assertEqual((result.returncode, result.stdout, result.stderr),
                         (1, '', 'home requires explicit migration\n'))
        self.assertEqual(self.snapshot(), before)

    def test_inspection_opens_only_directories_and_never_reads_credentials(self):
        (self.root / '.pi/agent').mkdir(parents=True)
        credential = self.root / '.pi/agent/auth.json'
        credential.write_text('secret-canary')
        before = self.snapshot()
        real_open = os.open

        def directories_only(path, flags, *args, **kwargs):
            self.assertTrue(flags & os.O_DIRECTORY)
            self.assertTrue(flags & os.O_NOFOLLOW)
            self.assertFalse(flags & (os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC))
            return real_open(path, flags, *args, **kwargs)

        with mock.patch.object(home.os, 'open', side_effect=directories_only), \
                mock.patch('builtins.open', side_effect=AssertionError('content read')):
            home.validate_home(self.root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), before)

    def test_ids_must_be_nonroot_numeric_and_not_reserved(self):
        before = self.snapshot()
        for value in [0, -1, -2, 4294967294, 4294967295, True, '1000']:
            for uid, gid in [(value, self.gid), (self.uid, value)]:
                with self.subTest(uid=uid, gid=gid), self.assertRaises(ValueError):
                    home.validate_home(self.root, uid, gid)
        self.assertEqual(self.snapshot(), before)

    def test_compatible_home_is_accepted_without_mutation(self):
        (self.root / '.pi/agent/sessions').mkdir(parents=True, mode=0o700)
        secret = self.root / '.pi/agent/auth.json'
        secret.write_text('sensitive-canary')
        secret.chmod(0o600)
        before = self.snapshot()
        home.validate_home(self.root, self.uid, self.gid)
        self.assertEqual(self.snapshot(), before)


if __name__ == '__main__':
    unittest.main()
