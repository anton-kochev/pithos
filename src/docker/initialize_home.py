"""Prepare only Pithos structural directories, never host mounts or user files.

Use directory descriptors and O_NOFOLLOW so privileged ownership changes cannot
follow user-controlled symlinks. Existing modes and all file contents survive.
"""
import os

UID, GID = 501, 20
flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
fds = []
try:
    parent = os.open('/home', flags)
    fds.append(parent)
    for name in ('pi', '.pi', 'agent', 'sessions'):
        try:
            os.mkdir(name, 0o700, dir_fd=parent)
        except FileExistsError:
            pass
        child = os.open(name, flags, dir_fd=parent)
        fds.append(child)
        info = os.fstat(child)
        if info.st_uid == 0:
            os.fchown(child, UID, GID)
        elif info.st_uid != UID:
            raise RuntimeError(f'{name}: unexpected directory owner {info.st_uid}; expected 0 or {UID}')
        parent = child
    # Check the same access the interactive process will have, without relaxing
    # existing modes or touching settings, credentials, or transcript files.
    os.setgroups([])
    os.setgid(GID)
    os.setuid(UID)
    for fd in fds[1:]:
        if not os.access('.', os.W_OK | os.X_OK, dir_fd=fd):
            raise PermissionError('Pithos home directory is not writable/searchable as 501:20')
finally:
    for fd in reversed(fds):
        os.close(fd)
