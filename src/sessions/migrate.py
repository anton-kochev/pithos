"""Offline, no-clobber import. Never follows symlinks or copies home credentials."""
import os
import shutil
import stat
import sys
import tempfile
from pathlib import Path

source = Path(sys.argv[1]) if len(sys.argv) > 1 else Path('/legacy/.pi/agent/sessions')
destination = Path(sys.argv[2]) if len(sys.argv) > 2 else Path('/sessions')
copied = skipped = 0


def check_directory(path):
    if not stat.S_ISDIR(path.lstat().st_mode):
        raise RuntimeError(f'Not a real directory: {path}')


def copy_tree(src, dst):
    global copied, skipped
    check_directory(src)
    dst.mkdir(mode=0o700, exist_ok=True)
    check_directory(dst)
    for entry in sorted(src.iterdir()):
        if src == source and entry.name == '.gitignore':
            continue  # Never replace the host privacy safeguard.
        target = dst / entry.name
        mode = entry.lstat().st_mode
        if stat.S_ISDIR(mode):
            copy_tree(entry, target)
        elif stat.S_ISREG(mode):
            if os.path.lexists(target):
                if not stat.S_ISREG(target.lstat().st_mode):
                    raise RuntimeError(f'Unsafe destination: {target}')
                skipped += 1
                continue
            # Publish a completed file atomically, without replacing another writer.
            fd, temporary = tempfile.mkstemp(prefix='.pithos-import-', dir=dst)
            try:
                with os.fdopen(fd, 'wb') as output, entry.open('rb') as input_file:
                    shutil.copyfileobj(input_file, output)
                info = entry.stat()
                os.utime(temporary, ns=(info.st_atime_ns, info.st_mtime_ns))
                try:
                    os.link(temporary, target)
                    copied += 1
                except FileExistsError:
                    skipped += 1
            finally:
                os.unlink(temporary)
        else:
            raise RuntimeError(f'Refusing symlink or special file: {entry}')


# Validate ancestors too: even the legacy volume is untrusted input.
for ancestor in [source.parent.parent, source.parent, source]:
    check_directory(ancestor)
copy_tree(source, destination)
print(f'Sessions imported: {copied}; skipped (already present): {skipped}. Legacy volume unchanged.')
