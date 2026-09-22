#!/usr/bin/env python3
"""Validate profile selection without reading real credentials or compiling."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

LAUNCHER = Path(__file__).resolve().parent / "run-stackforward"


class LauncherProfileTests(unittest.TestCase):
    def check_profile(self, profile, expected):
        with tempfile.TemporaryDirectory(prefix="router-launcher-test-") as tmp:
            root = Path(tmp).resolve()
            scripts = root / "scripts"
            scripts.mkdir()
            launcher = scripts / "run-stackforward"
            launcher.write_text(LAUNCHER.read_text())
            # Fixture values only. The launcher's credential behavior is unchanged.
            (root / ".env").write_text("HAARNESS_ROUTER_KEY=fixture-key\nHAARNESS_UI_USERNAME=fixture-user\nHAARNESS_UI_PASSWORD=fixture-password\n")
            secrets = root / "fixture.yml"
            secrets.write_text("fixture: true\n")
            stub = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
name = Path(sys.argv[0]).name
if name == 'ruby' and sys.argv[1] == '-rsecurerandom':
    sys.exit(0)
Path(os.environ['LAUNCHER_TEST_RECORD'] + '-' + name).write_text(json.dumps(sys.argv[1:]))
'''
            for name in ("cargo", "ruby"):
                path = scripts / name
                path.write_text(stub)
                path.chmod(0o755)
            env = dict(os.environ)
            env.pop("HAARNESS_ROUTER_PROFILE", None)
            env["PATH"] = str(scripts) + os.pathsep + env["PATH"]
            env["HAARNESS_STACKFORWARD_SECRETS"] = str(secrets)
            env["LAUNCHER_TEST_RECORD"] = str(root / "record")
            if profile is not None:
                env["HAARNESS_ROUTER_PROFILE"] = profile
            result = subprocess.run(["zsh", str(launcher), "--port", "9191", "--set", "web_ui.enabled=false"], env=env, capture_output=True, text=True)
            if expected is None:
                self.assertEqual(result.returncode, 2)
                self.assertFalse((root / "record-cargo").exists())
                self.assertFalse((root / "record-ruby").exists())
                return
            self.assertEqual(result.returncode, 0, result.stderr)
            cargo = json.loads((root / "record-cargo").read_text())
            self.assertEqual(cargo, ["build", "--profile", profile or "release", "--bin", "haarness-router", "--quiet"])
            ruby = json.loads((root / "record-ruby").read_text())
            self.assertEqual(ruby[2], str(root / "target" / expected / "haarness-router"))
            self.assertEqual(ruby[-4:], ["--port", "9191", "--set", "web_ui.enabled=false"])

    def test_default_release(self):
        self.check_profile(None, "release")

    def test_development(self):
        self.check_profile("dev", "debug")

    def test_debugger(self):
        self.check_profile("debugger", "debugger")

    def test_invalid_profile(self):
        self.check_profile("../../other", None)


if __name__ == "__main__":
    unittest.main()
