"""Exercise the installer's real jq resolver without network or /opt writes.

Run with: python3 tests/node_install_test.py
Requires jq, as does the installer.
"""
import json
import pathlib
import re
import subprocess
import unittest


INSTALLER = pathlib.Path(__file__).resolve().parents[1] / "toolchains/node-install.sh"


def resolve(requested, releases, platform="linux-arm64"):
    script = INSTALLER.read_text()
    query = re.search(
        r"jq -r --arg requested \"\$requested\" --arg platform \"\$platform\" '(.*?)'",
        script,
        re.DOTALL,
    )
    if query is None:
        raise AssertionError("Could not locate the installer's jq resolver")
    result = subprocess.run(
        ["jq", "-r", "--arg", "requested", requested, "--arg", "platform", platform,
         query.group(1)],
        input=json.dumps(releases), text=True, capture_output=True,
    )
    if result.returncode != 0:
        raise AssertionError(f"Resolver failed: {result.stderr}")
    return result.stdout.strip()


def release(version, platform="linux-arm64"):
    return {"version": version, "files": [platform]}


class NodeResolverTests(unittest.TestCase):
    def test_exact_version_ignores_prereleases(self):
        releases = [release("v0.1.0-alpha"), release("v24.20.0"),
                    release("v24.21.0-rc.1")]
        self.assertEqual(resolve("24.20.0", releases), "v24.20.0")

    def test_partial_versions_choose_newest_numeric_supported_release(self):
        releases = [release("v24.21.0-rc.1"), release("v24.9.0"),
                    release("v24.20.9"), release("v24.20.10"),
                    release("v24.20.11", "linux-x64"), release("v25.0.0")]
        for requested in ["24", "24.20"]:
            with self.subTest(requested=requested):
                self.assertEqual(resolve(requested, releases), "v24.20.10")

    def test_prerelease_only_match_returns_no_release(self):
        self.assertEqual(resolve("24.20", [release("v24.20.0-alpha"),
                                          release("v24.20.0-rc.1")]), "")

    def test_stable_release_for_other_architecture_returns_no_release(self):
        self.assertEqual(resolve("24.20.0", [release("v24.20.0", "linux-x64")]), "")


if __name__ == "__main__":
    unittest.main()
