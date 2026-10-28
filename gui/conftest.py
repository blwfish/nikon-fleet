"""Shared test fixtures for the GUI's tests."""
import os
from pathlib import Path

import pytest

from fleet_cli import CliNotFound, find_cli

GUI_DIR = Path(__file__).resolve().parent
PROJECT = GUI_DIR.parent
FIXTURES = GUI_DIR / "fixtures"


def required(*names: str, env=None) -> bool:
    """Whether a missing prerequisite must FAIL the test instead of skipping it:
    any of *names* set to a non-empty value other than `0`/`false`. Mirrors
    `python_required()` in tests/gui_cli_argv_parity.rs (CI plus a REQUIRE_* switch)."""
    env = os.environ if env is None else env
    return any(env.get(n, "") not in ("", "0", "false") for n in names)


def skip_or_fail(reason: str, *names: str) -> None:
    if required("CI", *names):
        pytest.fail(f"{reason} (required: CI or {' / '.join(names)} is set)")
    pytest.skip(reason)


@pytest.fixture(scope="session")
def real_cli() -> str:
    """Path of the built nikon-fleet binary (or $NIKON_FLEET_CLI). Skips the
    test when there is none, unless CI or REQUIRE_CLI is set, which makes it a
    failure. Only camera-free commands are run with it."""
    try:
        return str(find_cli(PROJECT))
    except CliNotFound as e:
        skip_or_fail(f"no nikon-fleet binary: {e}", "REQUIRE_CLI")
