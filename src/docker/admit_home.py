"""Read-only existing-home inspection; never migration or run authorization.

Linux/POSIX directory-descriptor APIs are required (the embedded target is
Linux). No credential contents, ordinary symlink targets, or account databases
are read. Detected races fail closed, but callers still need exclusive access:
metadata observation is not an atomic snapshot and cannot authorize a later run.
"""


import os
import stat
import time


FAILURE = 'home requires explicit migration'
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


class Rejected(ValueError):
    """The static failure plus a fixed reason code; never a path or input."""

    def __init__(self, reason):
        super().__init__(FAILURE)
        self.reason = reason


def _signature(info):
    # atime may change as a result of directory enumeration; it is not evidence
    # of mutation. Never open file contents or resolve ordinary symlink targets.
    return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_gid,
            info.st_nlink, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def _open_root(root):
    # Component-relative opens also reject a symlink before the final component
    # and a root symlink disguised by a trailing slash.
    path = os.fspath(root)
    if (not isinstance(path, str) or not path.startswith('/') or len(path) > 4096
            or '\x00' in path):
        raise Rejected('input')
    parts = [part for part in path.split('/') if part]
    if not parts or len(parts) > 64 or any(part in ('.', '..') for part in parts):
        raise Rejected('input')
    fd = os.open('/', DIR_FLAGS)
    try:
        for part in parts:
            child = os.open(part, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def validate_home(root, uid, gid, *, browser=False, max_entries=100000, max_depth=64,
                  max_seconds=30):
    """Inspect an existing absolute root, returning None or raising ValueError.

    Root is depth zero and excluded from the 100000-descendant budget. Two
    streamed passes each have that cap and share one cooperative 30s deadline;
    blocking kernel calls cannot be interrupted by this cooperative check.
    Optional limits can tighten, but never relax, these hard caps.
    """
    if any(type(value) is not int or not 0 < value < 4294967294 for value in (uid, gid)):
        raise Rejected('input')

    if (type(max_entries) is not int or not 0 <= max_entries <= 100000
            or type(max_depth) is not int or not 0 <= max_depth <= 64
            or type(max_seconds) not in (int, float) or not 0 < max_seconds <= 30):
        raise Rejected('input')
    deadline = time.monotonic() + max_seconds
    count = 0
    observed = {}

    def time_check():
        if time.monotonic() >= deadline:
            raise Rejected('timeout')

    structural = {(), ('.pi',), ('.pi', 'agent'), ('.pi', 'agent', 'sessions')}
    if browser:
        structural.update({('.agents',), ('.agents', 'skills'),
                           ('.agents', 'skills', 'pithos-browser')})

    def check(info, path):
        if (info.st_uid, info.st_gid) != (uid, gid):
            raise Rejected('owner')
        if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)
                or stat.S_ISLNK(info.st_mode)):
            raise Rejected('special-file')
        # Non-directory hardlinks may alias data outside this home, including
        # hardlinked symlinks. Directory link counts include child directories.
        if not stat.S_ISDIR(info.st_mode) and info.st_nlink != 1:
            raise Rejected('hardlink')
        if path in structural and not stat.S_ISDIR(info.st_mode):
            raise Rejected('layout')
        if stat.S_ISDIR(info.st_mode):
            required = 0o700 if path in structural else 0o500
            if info.st_mode & required != required:
                raise Rejected('permissions')

    def observe(info, path, verifying):
        check(info, path)
        signature = _signature(info)
        if verifying:
            if observed.pop(path, None) != signature:
                raise Rejected('changed')
        else:
            if path in observed:
                raise Rejected('changed')
            observed[path] = signature

    def scan(fd, path, expected, verifying):
        nonlocal count
        time_check()
        if _signature(os.fstat(fd)) != _signature(expected):
            raise Rejected('changed')
        with os.scandir(fd) as entries:
            for entry in entries:
                time_check()
                count += 1
                if count > max_entries or len(path) + 1 > max_depth:
                    raise Rejected('too-large')
                if browser and path == ('.agents', 'skills', 'pithos-browser'):
                    raise Rejected('layout')
                info = os.stat(entry.name, dir_fd=fd, follow_symlinks=False)
                child_path = path + (entry.name,)
                observe(info, child_path, verifying)
                if stat.S_ISDIR(info.st_mode):
                    child = os.open(entry.name, DIR_FLAGS, dir_fd=fd)
                    try:
                        scan(child, child_path, info, verifying)
                    finally:
                        os.close(child)
                after = os.stat(entry.name, dir_fd=fd, follow_symlinks=False)
                if _signature(after) != _signature(info):
                    raise Rejected('changed')
        if _signature(os.fstat(fd)) != _signature(expected):
            raise Rejected('changed')

    try:
        # A second bounded metadata pass catches earlier leaves changing during
        # a later subtree scan. Reopen from the root path to check its binding.
        # This detects observed races, not an atomic snapshot or a run permit.
        for verifying in (False, True):
            count = 0
            time_check()
            fd = _open_root(root)
            try:
                info = os.fstat(fd)
                observe(info, (), verifying)
                scan(fd, (), info, verifying)
            finally:
                os.close(fd)
        if observed:
            raise Rejected('changed')
        fd = _open_root(root)
        try:
            if _signature(os.fstat(fd)) != _signature(info):
                raise Rejected('changed')
        finally:
            os.close(fd)
        time_check()
    except OSError:
        raise Rejected('unreadable') from None


def main(argv):
    """Strict positional CLI used by the Rust-generated `python3 -c` argv."""
    if len(argv) not in (3, 4) or (len(argv) == 4 and argv[3] != '--browser'):
        raise Rejected('input')
    ids = argv[1:3]
    if any(not 1 <= len(value) <= 10 or not value.isascii() or not value.isdecimal()
           or value.startswith('0') for value in ids):
        raise Rejected('input')
    validate_home(argv[0], int(ids[0]), int(ids[1]), browser=len(argv) == 4)


if __name__ == '__main__':
    import sys
    try:
        main(sys.argv[1:])
    except Rejected as error:
        # Only the fixed reason code is added: never paths, credentials,
        # exception messages or tracebacks.
        sys.stderr.write(f'{FAILURE}: {error.reason}\n')
        sys.exit(1)
    except (Exception, KeyboardInterrupt):
        # This executable boundary must never echo paths, credentials, exception
        # messages, or tracebacks, including unexpected filesystem failures.
        sys.stderr.write(FAILURE + '\n')
        sys.exit(1)
    print('home inspection passed')
