"""Fixed image-build helper, never a runtime home migration tool.

Importing this module performs no I/O. Only the explicit image-build entrypoint
may use the image's account databases. Tests supply isolated fixture roots.
Requires an exclusive, trusted image build filesystem without runtime mounts or
concurrent writers; this is not a race-safe host/volume migration API. Any I/O
failure aborts the build (discard the failed layer), not a transactional repair.
"""


import os
from pathlib import Path
import stat


ROLE_ACCOUNTS = {
    "pi": ("/home/pi", "/bin/bash"),
    "browser": ("/tmp/browser-home", "/bin/sh"),
}


def parse_database(text, fields):
    entries = [line.split(":") for line in text.splitlines()]
    names, ids = set(), set()
    for entry in entries:
        if (len(entry) != fields or not entry[0] or entry[0] in names
                or any(char in text for char in "\x00\r")
                or not entry[2].isascii() or not entry[2].isdecimal()
                or str(int(entry[2])) != entry[2] or int(entry[2]) >= 4294967294
                or entry[2] in ids):
            raise ValueError("invalid or ambiguous account database")
        if fields == 7 and (not entry[3].isascii() or not entry[3].isdecimal()
                            or str(int(entry[3])) != entry[3] or int(entry[3]) >= 4294967294):
            raise ValueError("invalid account group")
        names.add(entry[0])
        ids.add(entry[2])
    return entries


def plan_accounts(passwd, group, role, uid, gid):
    """Return replacement account database text without performing I/O."""
    if role not in ("pi", "browser") or any(type(value) is not int or not 0 < value < 4294967294
                                            for value in (uid, gid)):
        raise ValueError("invalid image identity")
    accounts = parse_database(passwd, 7)
    groups = parse_database(group, 4)
    target = [account for account in accounts if account[0] == role]
    if (len(target) != 1 or target[0][1] != "x" or int(target[0][2]) == 0
            or int(target[0][3]) == 0 or tuple(target[0][5:7]) != ROLE_ACCOUNTS[role]):
        raise ValueError("unexpected image role account")
    collisions = [account for account in accounts if int(account[2]) == uid and account[0] != role]
    if collisions:
        # Exact shape from the pinned official Debian Node image. Only remove
        # the passwd entry, never its group, shadow data, home or other files.
        known_node = ["node", "x", "1000", "1000", "", "/home/node", "/bin/bash"]
        if (role != "browser" or uid != 1000 or collisions != [known_node]
                or [entry for entry in groups if entry[0] == "node"] != [["node", "x", "1000", ""]]):
            raise ValueError("UID collision")
        accounts.remove(known_node)
    if not any(int(entry[2]) == gid for entry in groups):
        if any(entry[0] == f"pithos-{gid}" for entry in groups):
            raise ValueError("group name collision")
        group = group.rstrip("\n") + f"\npithos-{gid}:x:{gid}:\n"
    for account in accounts:
        if account[0] == role:
            account[2:4] = [str(uid), str(gid)]
    return "".join(":".join(account) + "\n" for account in accounts), group


def check_ancestors(root, path):
    for ancestor in [path, *path.parents]:
        if ancestor.is_symlink():
            raise ValueError("symlink image path or ancestor")
        if ancestor == root:
            break


def apply_image(root, role, uid, gid, *, chown=os.chown):
    """Build-only boundary; tests inject a private root and record-only chown.

    Preflight the account plan and every selected tree before ownership changes.
    Symlinks inside trees are left alone (npm/rustup use them); their targets are
    never followed. Regular-file hardlinks are allowed only when every link is
    accounted for inside these fixed trees. No search for files by old UID and no usermod/userdel side
    effects on arbitrary homes, mail spools or supplementary groups.
    """
    passwd_path, group_path = root / "etc/passwd", root / "etc/group"
    for path in (passwd_path, group_path):
        check_ancestors(root, path)
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise ValueError("unsafe account database file")
    passwd, group = plan_accounts(passwd_path.read_text(), group_path.read_text(), role, uid, gid)
    if role == "browser":
        trees = [root / "tmp/browser-home"]
        check_ancestors(root, trees[0])
        trees[0].mkdir(exist_ok=True)
    else:
        trees = [root / name for name in ("home/pi", "opt/pi-npm", "opt/cargo", "opt/rustup")]
    for tree in trees:
        check_ancestors(root, tree)
    ownership = []
    links = {}
    for tree in trees:
        if tree in (root / "opt/cargo", root / "opt/rustup") and not tree.exists():
            continue
        if not tree.is_dir():
            raise ValueError("expected image directory")
        for path in [tree, *tree.rglob("*")]:
            if path.is_symlink():
                continue
            metadata = path.lstat()
            mode = metadata.st_mode
            if not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)):
                raise ValueError("special image file")
            if stat.S_ISREG(mode):
                entry = links.setdefault((metadata.st_dev, metadata.st_ino), [metadata.st_nlink, 0])
                if entry[0] != metadata.st_nlink:
                    raise ValueError("changing image hardlink count")
                entry[1] += 1
            ownership.append((path, mode))
    if any(expected != observed for expected, observed in links.values()):
        raise ValueError("hardlink outside image ownership trees")
    for path, mode in ownership:
        chown(path, uid, gid, follow_symlinks=False)
        path.chmod((mode & 0o777) | (0o700 if stat.S_ISDIR(mode) else 0o600))
    passwd_path.write_text(passwd)
    group_path.write_text(group)


def main(argv, *, apply=apply_image, effective_uid=os.geteuid):
    """Explicit build entrypoint; injected boundaries are for offline tests only."""
    if len(argv) != 4 or argv[0] != "--image-build":
        raise ValueError("expected --image-build ROLE UID GID")
    if effective_uid() != 0:
        raise ValueError("image build requires root")
    apply(Path("/"), argv[1], int(argv[2]), int(argv[3]))


if __name__ == "__main__":
    import sys
    try:
        main(sys.argv[1:])
    except (ValueError, OSError) as error:
        sys.exit(f"identity image build failed: {error}")
