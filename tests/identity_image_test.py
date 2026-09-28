"""Offline fixtures only: never invoke the real host account/chown boundary."""
import os
import pathlib
import runpy
import shutil
import tempfile
import unittest

helper = runpy.run_path(str(pathlib.Path(__file__).parents[1] / "src/docker/identity_image.py"))
plan_accounts = helper["plan_accounts"]
ROOT = "root:x:0:0:root:/root:/bin/bash\n"
PI = "pi:x:501:20::/home/pi:/bin/bash\n"
BROWSER = "browser:x:501:20::/tmp/browser-home:/bin/sh\n"
NODE = "node:x:1000:1000::/home/node:/bin/bash\n"
GROUPS = "root:x:0:\ndialout:x:20:\nnode:x:1000:\n"


class AccountTests(unittest.TestCase):
    def test_pi_remap_reuses_numeric_group_without_renumbering(self):
        passwd, group = plan_accounts(ROOT + PI, GROUPS, "pi", 12345, 1000)
        self.assertEqual((passwd, group), (
            ROOT + "pi:x:12345:1000::/home/pi:/bin/bash\n", GROUPS))

    def test_new_group_does_not_renumber_existing_role_group(self):
        groups = GROUPS + "pi:x:501:other\n"
        _, group = plan_accounts(ROOT + PI, groups, "pi", 12345, 23456)
        self.assertEqual(group, groups + "pithos-23456:x:23456:\n")

    def test_unknown_uid_collision_is_rejected(self):
        for role, account in [("pi", PI), ("browser", BROWSER)]:
            with self.subTest(role=role), self.assertRaisesRegex(ValueError, "UID collision"):
                plan_accounts(ROOT + account + "other:x:12345:20::/srv/other:/bin/sh\n",
                              GROUPS, role, 12345, 20)

    def test_browser_removes_only_verified_colliding_node_account(self):
        result = plan_accounts(ROOT + NODE + BROWSER, GROUPS, "browser", 1000, 1000)
        self.assertEqual(result, (
            ROOT + "browser:x:1000:1000::/tmp/browser-home:/bin/sh\n", GROUPS))

    def test_node_exception_is_exact_role_scoped_and_collision_only(self):
        for altered in [NODE.replace("node:", "other:"), NODE.replace("/home/node", "/srv/data"),
                        NODE.replace("/bin/bash", "/bin/sh"), NODE.replace("::", ":Node:"),
                        NODE.replace(":x:", ":!:")]:
            with self.assertRaises(ValueError):
                plan_accounts(ROOT + altered + BROWSER, GROUPS, "browser", 1000, 20)
        with self.assertRaises(ValueError):
            plan_accounts(ROOT + NODE + PI, GROUPS, "pi", 1000, 20)
        with self.assertRaises(ValueError):
            plan_accounts(ROOT + NODE + BROWSER, GROUPS.replace("node:x:1000:", "node:x:1000:other"),
                          "browser", 1000, 20)
        passwd, _ = plan_accounts(ROOT + NODE + BROWSER, GROUPS, "browser", 12345, 20)
        self.assertIn(NODE, passwd)

    def test_invalid_ids_and_roles_are_rejected(self):
        for role, uid, gid in [("pi", 0, 20), ("pi", 501, 0), ("pi", -1, 20),
                               ("pi", 501, 4294967294), ("pi", 4294967295, 20),
                               ("pi", 4294967296, 20), ("pi", True, 20),
                               ("pi", 501, "20"), ("other", 501, 20)]:
            with self.assertRaises(ValueError):
                plan_accounts(ROOT + PI, GROUPS, role, uid, gid)

    def test_ambiguous_or_unexpected_accounts_fail_closed(self):
        cases = [
            (ROOT, GROUPS),
            (ROOT + PI + PI, GROUPS),
            (ROOT + PI + "other:x:501:20::/srv/other:/bin/sh\n", GROUPS),
            (ROOT + PI.replace("/home/pi", "/srv/private"), GROUPS),
            (ROOT + PI.replace(":501:", ":0:"), GROUPS),
            (ROOT + PI.replace(":x:", ":password:"), GROUPS),
            (ROOT + PI + "broken\n", GROUPS),
            (ROOT + PI, GROUPS + "pithos-23456:x:23457:\n"),
            (ROOT + PI, GROUPS + "node:x:1234:\n"),
            (ROOT + PI, GROUPS + "other:x:1000:\n"),
        ]
        for passwd, groups in cases:
            with self.assertRaises(ValueError):
                plan_accounts(passwd, groups, "pi", 12345, 23456)


class EntrypointTests(unittest.TestCase):
    def test_explicit_build_entrypoint_delegates_fixed_root_and_typed_ids(self):
        calls = []
        helper["main"](["--image-build", "pi", "12345", "23456"],
                       apply=lambda *args: calls.append(args), effective_uid=lambda: 0)
        self.assertEqual(calls, [(pathlib.Path("/"), "pi", 12345, 23456)])

    def test_non_root_entrypoint_rejects_without_reaching_image_boundary(self):
        calls = []
        with self.assertRaisesRegex(ValueError, "root"):
            helper["main"](["--image-build", "pi", "12345", "23456"],
                           apply=lambda *args: calls.append(args), effective_uid=lambda: 501)
        self.assertEqual(calls, [])


class ImageFixtureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.ownership = []
        self.put("etc/passwd", ROOT + PI)
        self.put("etc/group", GROUPS)
        self.put("home/pi/settings", "private defaults")
        self.put("opt/pi-npm/bin/pi", "pi")
        self.put("opt/cargo/bin/cargo", "cargo")
        self.put("opt/rustup/toolchains/rustc", "rustc")
        self.put("workspace/source", "untouched")
        self.put("home/node/keep", "not deleted")
        self.put("opt/unrelated/keep", "not changed")

    def put(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        return path

    def chown(self, path, uid, gid, *, follow_symlinks):
        self.assertFalse(follow_symlinks)
        self.ownership.append((str(path.relative_to(self.root)), uid, gid))

    def apply(self, role="pi", uid=12345, gid=23456):
        helper["apply_image"](self.root, role, uid, gid, chown=self.chown)

    def test_pi_ownership_is_limited_to_fixed_image_trees(self):
        self.apply()
        expected = {"home/pi", "home/pi/settings", "opt/pi-npm", "opt/pi-npm/bin",
                    "opt/pi-npm/bin/pi", "opt/cargo", "opt/cargo/bin", "opt/cargo/bin/cargo",
                    "opt/rustup", "opt/rustup/toolchains", "opt/rustup/toolchains/rustc"}
        self.assertEqual(set(self.ownership), {(path, 12345, 23456) for path in expected})
        self.assertIn("pi:x:12345:23456:", (self.root / "etc/passwd").read_text())
        self.assertEqual((self.root / "workspace/source").read_text(), "untouched")

    def test_absent_optional_toolchains_are_not_created_or_chowned(self):
        for name in ["opt/cargo", "opt/rustup"]:
            shutil.rmtree(self.root / name)
        self.apply()
        self.assertFalse(any(path.startswith(("opt/cargo", "opt/rustup"))
                             for path, _, _ in self.ownership))
        self.assertFalse((self.root / "opt/cargo").exists())
        self.assertFalse((self.root / "opt/rustup").exists())

    def test_browser_creates_only_its_home_and_preserves_node_files(self):
        self.put("etc/passwd", ROOT + NODE + BROWSER)
        (self.root / "tmp").mkdir()
        self.apply("browser", 1000, 1000)
        self.assertEqual(self.ownership, [("tmp/browser-home", 1000, 1000)])
        self.assertTrue((self.root / "tmp/browser-home").is_dir())
        self.assertEqual((self.root / "home/node/keep").read_text(), "not deleted")
        self.assertNotIn(NODE, (self.root / "etc/passwd").read_text())

    def test_owned_files_become_owner_writable_without_broadening_other_access(self):
        path = self.root / "opt/pi-npm/bin/pi"
        path.chmod(0o450)
        self.apply()
        self.assertEqual(path.stat().st_mode & 0o777, 0o650)

    def test_symlink_tree_root_is_rejected_before_any_mutation(self):
        shutil.rmtree(self.root / "home/pi")
        (self.root / "home/pi").symlink_to(self.root / "workspace", target_is_directory=True)
        before = (self.root / "etc/passwd").read_bytes()
        with self.assertRaises(ValueError):
            self.apply()
        self.assertEqual(self.ownership, [])
        self.assertEqual((self.root / "etc/passwd").read_bytes(), before)

    def test_symlink_ancestor_is_rejected_before_any_mutation(self):
        (self.root / "opt").rename(self.root / "elsewhere")
        (self.root / "opt").symlink_to(self.root / "elsewhere", target_is_directory=True)
        with self.assertRaises(ValueError):
            self.apply()
        self.assertEqual(self.ownership, [])
        self.assertEqual((self.root / "etc/passwd").read_text(), ROOT + PI)

    def test_internal_symlinks_never_chown_or_chmod_their_targets(self):
        target = self.root / "workspace/source"
        target.chmod(0o400)
        (self.root / "opt/pi-npm/external").symlink_to(target)
        (self.root / "opt/pi-npm/external-dir").symlink_to(self.root / "workspace", target_is_directory=True)
        self.apply()
        self.assertEqual(target.stat().st_mode & 0o777, 0o400)
        self.assertFalse(any("external" in path for path, _, _ in self.ownership))

    def test_image_local_npm_hardlinks_are_supported_without_expanding_ownership(self):
        first = self.put("opt/pi-npm/lib/node_modules/esbuild/bin/esbuild", "executable")
        second = self.root / "opt/pi-npm/bin/esbuild"
        os.link(first, second)
        first.chmod(0o500)
        self.apply()
        self.assertEqual(first.stat().st_ino, second.stat().st_ino)
        self.assertEqual(first.stat().st_nlink, 2)
        self.assertEqual(first.stat().st_mode & 0o777, 0o700)
        for path in (first, second):
            self.assertIn((str(path.relative_to(self.root)), 12345, 23456), self.ownership)
        self.assertFalse(any(path.startswith("workspace") for path, _, _ in self.ownership))

    def test_late_hardlink_escape_is_rejected_before_any_mutation(self):
        os.link(self.root / "workspace/source", self.root / "opt/rustup/escape")
        with self.assertRaises(ValueError):
            self.apply()
        self.assertEqual(self.ownership, [])
        self.assertEqual((self.root / "etc/passwd").read_text(), ROOT + PI)

    def test_account_database_symlink_is_rejected_without_mutation(self):
        original = self.root / "etc/passwd"
        original.rename(self.root / "saved-passwd")
        original.symlink_to(self.root / "saved-passwd")
        with self.assertRaises(ValueError):
            self.apply()
        self.assertEqual(self.ownership, [])
        self.assertEqual((self.root / "saved-passwd").read_text(), ROOT + PI)

    def test_browser_symlink_parent_does_not_create_home_before_rejection(self):
        self.put("etc/passwd", ROOT + BROWSER)
        (self.root / "tmp").symlink_to(self.root / "workspace", target_is_directory=True)
        with self.assertRaises(ValueError):
            self.apply("browser")
        self.assertFalse((self.root / "workspace/browser-home").exists())

    def test_special_file_is_rejected_before_any_ownership_changes(self):
        os.mkfifo(self.root / "opt/rustup/pipe")
        with self.assertRaises(ValueError):
            self.apply()
        self.assertEqual(self.ownership, [])
        self.assertEqual((self.root / "etc/passwd").read_text(), ROOT + PI)

    def test_ownership_permission_failure_is_not_reported_as_success(self):
        def denied(*args, **kwargs):
            raise PermissionError("injected ownership boundary denial")
        with self.assertRaises(PermissionError):
            helper["apply_image"](self.root, "pi", 12345, 23456, chown=denied)
        self.assertEqual((self.root / "etc/passwd").read_text(), ROOT + PI)

    def test_identity_collision_leaves_databases_and_trees_untouched(self):
        passwd = ROOT + PI + "other:x:12345:20::/srv/other:/bin/sh\n"
        self.put("etc/passwd", passwd)
        with self.assertRaises(ValueError):
            self.apply()
        self.assertEqual(self.ownership, [])
        self.assertEqual((self.root / "etc/passwd").read_text(), passwd)
        self.assertEqual((self.root / "etc/group").read_text(), GROUPS)

    def test_required_image_tree_cannot_be_a_regular_file(self):
        shutil.rmtree(self.root / "home/pi")
        self.put("home/pi", "not a home")
        with self.assertRaises(ValueError):
            self.apply()
        self.assertEqual(self.ownership, [])


if __name__ == "__main__":
    unittest.main()
