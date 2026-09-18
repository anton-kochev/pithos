"""Reserve one empty, stable discovery mount without replacing user resources."""
import os


def prepare(root='/home/pi'):
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    fds = []
    try:
        parent = os.open(root, flags)
        fds.append(parent)
        for name in ('.agents', 'skills', 'pithos-browser'):
            try:
                os.mkdir(name, 0o700, dir_fd=parent)
            except FileExistsError:
                pass
            child = os.open(name, flags, dir_fd=parent)
            fds.append(child)
            if os.fstat(child).st_uid != os.getuid():
                raise RuntimeError('browser skill mount ancestor has a different owner')
            parent = child
        if os.listdir(parent):
            raise RuntimeError('browser skill mount conflicts with existing content; move it explicitly before enabling browsing')
    finally:
        for fd in reversed(fds):
            os.close(fd)


if __name__ == '__main__':
    prepare()
