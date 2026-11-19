# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Local settings shared by dev.ps1 and the Python tools.

A setting is read from the environment first, then from `dev.env` (KEY=VALUE lines, `#` comments) in the
repository root, or in the main checkout when this is a linked git worktree, then falls back to a default.
CONTRIBUTING.md lists the settings. Tools import this module with

    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))  # tools/
    import devenv
"""

import os
import subprocess
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]

_file_settings = None


def _settings_file():
    own = REPO / "dev.env"
    if own.exists():
        return own
    try:
        common = subprocess.run(
            ["git", "-C", str(REPO), "rev-parse", "--path-format=absolute", "--git-common-dir"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    main = Path(common).parent / "dev.env"
    return main if main.exists() else None


def _load():
    global _file_settings
    if _file_settings is None:
        _file_settings = {}
        path = _settings_file()
        if path:
            for line in path.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                key, value = line.split("=", 1)
                _file_settings[key.strip()] = value.strip()
    return _file_settings


def setting(name, default=None):
    """The value of a setting: the environment, then dev.env, then `default`."""
    value = os.environ.get(name)
    if value:
        return value
    return _load().get(name, default)


def apply_to_environ(*names):
    """Copy settings from dev.env into os.environ (for child processes) where the environment lacks them."""
    for name in names or tuple(_load()):
        if not os.environ.get(name) and name in _load():
            os.environ[name] = _load()[name]


def dev_dir():
    """SNOWLARK_DEV_DIR: scratch space for build output, kept builds and tool results.

    Defaults to `.dev` in the repository (git-ignored)."""
    return Path(setting("SNOWLARK_DEV_DIR", str(REPO / ".dev")))


def wpt_dir():
    """SNOWLARK_WPT: the web-platform-tests checkout. Defaults to `wpt` next to the repository."""
    return Path(setting("SNOWLARK_WPT", str(REPO.parent / "wpt")))


def speedometer_dir():
    """SNOWLARK_SPEEDOMETER: a local copy of Speedometer 3.1. Defaults to `<dev dir>/speedometer-3.1`."""
    return Path(setting("SNOWLARK_SPEEDOMETER", str(dev_dir() / "speedometer-3.1")))


def window_pos():
    """SNOWLARK_TEST_WINDOW_POS: where the tools that drive a real window open it, as `x,y` in screen pixels
    (default 100,100). Point it at a monitor you are not working on."""
    x, y = setting("SNOWLARK_TEST_WINDOW_POS", "100,100").split(",")
    return int(x), int(y)


def cargo_home():
    """The crate cache dev.ps1 builds with: CARGO_HOME, else `<SNOWLARK_DEV_DIR>/.cargo-home`, else ~/.cargo."""
    home = os.environ.get("CARGO_HOME")
    if home:
        return Path(home)
    dev = setting("SNOWLARK_DEV_DIR")
    return Path(dev) / ".cargo-home" if dev else Path.home() / ".cargo"


def exe(profile="iter", cli=True):
    """The Snowlark executable a build of this checkout makes."""
    name = "snowlark-cli.exe" if cli else "snowlark.exe"
    target = setting("CARGO_TARGET_DIR", str(REPO / "target"))
    return Path(target) / profile / name
