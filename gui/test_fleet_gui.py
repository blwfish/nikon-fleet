"""The Tk wiring, driven headless-friendly: a withdrawn root window, a fake
`nikon-fleet` (selected with $NIKON_FLEET_CLI) that logs its argv and answers
from a spec, and the message boxes replaced by recorders. Skipped when Tk has
no display. No camera and no real CLI is involved."""
import gc
import json
import stat
import sys
import time

import pytest

import fleet_gui
from conftest import FIXTURES, PROJECT, skip_or_fail
from fleet_cli import ENV_CLI

FAKE = """#!{python}
import json, os, sys, time
args = sys.argv[1:]
spec = json.load(open(os.environ["FAKE_SPEC"]))
rest = [a for a in args if not a.startswith("--no-usb-reset") and not a.startswith("--data-dir=")]
key = rest[0]
if key in ("firmware", "settings", "ref"):
    key += " " + rest[1]
stdin = sys.stdin.read() if "--password-stdin" in args else None
counts_path = os.environ["FAKE_SPEC"] + ".counts"
counts = json.load(open(counts_path)) if os.path.exists(counts_path) else {{}}
n = counts.get(key, 0)
counts[key] = n + 1
json.dump(counts, open(counts_path, "w"))
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(json.dumps({{"args": args, "stdin": stdin}}) + "\\n")
entry = spec.get(key)
if entry is None:
    # a test that forgot to say what this command does must not pass by accident
    sys.stderr.write("unspecified fake command: " + key + "\\n")
    sys.exit(99)
if isinstance(entry, list):
    entry = entry[min(n, len(entry) - 1)]
time.sleep(entry.get("sleep", 0))
sys.stdout.write(entry.get("stdout", ""))
sys.stderr.write(entry.get("stderr", ""))
sys.stdout.flush()
sys.stderr.flush()
if "signal" in entry:
    os.kill(os.getpid(), entry["signal"])
    time.sleep(5)
sys.exit(entry.get("rc", 0))
"""


def _fixture(name: str) -> str:
    return (FIXTURES / name).read_text(encoding="utf-8")


@pytest.fixture
def fake_cli(tmp_path, monkeypatch):
    script = tmp_path / "fake-nikon-fleet"
    script.write_text(FAKE.format(python=sys.executable))
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    log = tmp_path / "calls.log"
    spec = tmp_path / "spec.json"
    data_dir = tmp_path / "data"
    data_dir.mkdir()
    monkeypatch.setenv(ENV_CLI, str(script))
    monkeypatch.setenv("FAKE_SPEC", str(spec))
    monkeypatch.setenv("FAKE_LOG", str(log))

    class Fake:
        def __init__(self):
            self.data_dir = data_dir
            self.spec = {
                "settings show": {"stdout": json.dumps({
                    "settings_path": "/s.json", "settings_exist": True, "data_dir": str(data_dir),
                    "effective_data_dir": str(data_dir), "keys": {}})},
                "ls": {"stdout": _fixture("ls_reference_parity.json")},
                "firmware ls": {"stdout": _fixture("firmware_ls.json")},
            }
            self.write()

        def write(self):
            spec.write_text(json.dumps(self.spec))
            counts = tmp_path / "spec.json.counts"
            if counts.exists():
                counts.unlink()

        def calls(self):
            if not log.exists():
                return []
            return [json.loads(line) for line in log.read_text().splitlines()]

        def commands(self):
            """Each call's args without the global options."""
            return [[a for a in c["args"] if not a.startswith(("--no-usb-reset", "--data-dir="))]
                    for c in self.calls()]

    return Fake()


@pytest.fixture
def tk_root():
    try:
        import tkinter as tk
    except ImportError as e:
        skip_or_fail(f"tkinter is not available: {e}", "REQUIRE_DISPLAY")
    try:
        root = tk.Tk()
    except tk.TclError as e:
        skip_or_fail(f"no display: {e}", "REQUIRE_DISPLAY")
    root.withdraw()
    yield root
    root.destroy()
    # Free the dead app's Tk variables here, on the main thread: left to the
    # cyclic collector they can be finalised on a worker thread, which Tk refuses.
    gc.collect()


@pytest.fixture
def dialogs(monkeypatch):
    """Replace the modal dialogs with recorders. `answers` sets what askyesno
    and askstring return."""
    class Rec:
        def __init__(self):
            self.shown, self.strings, self.asked = [], [], []
            self.yes = True

    rec = Rec()

    def recorder(kind):
        def f(title, message, **kw):
            rec.shown.append((kind, title, message))
            return rec.yes if kind == "askyesno" else None
        return f
    for kind in ("showerror", "showinfo", "showwarning", "askyesno"):
        monkeypatch.setattr(fleet_gui.messagebox, kind, recorder(kind))

    def askstring(title, prompt, **kw):
        rec.asked.append((title, prompt, kw.get("initialvalue")))
        return rec.strings.pop(0) if rec.strings else None
    monkeypatch.setattr(fleet_gui.simpledialog, "askstring", askstring)
    return rec


def pump(root, until, timeout=60.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        root.update()
        if until():
            return
        time.sleep(0.01)
    raise AssertionError("timed out waiting for the GUI")


def settle(root, app):
    """Run the event loop until no CLI job is in flight."""
    pump(root, lambda: not app._cli.busy and not app._cli_poll._ticking)


def make_app(root, fake):
    app = fleet_gui.FleetApp(root)
    pump(root, lambda: app._fw_listing is not None)
    settle(root, app)
    return app


def notes(app) -> str:
    return app._notes.get("1.0", "end")


class TestStartup:
    def test_the_startup_chain_reads_settings_then_listings_through_the_cli(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        assert fake_cli.commands() == [["settings", "show", "--json"], ["ls", "--json"], ["firmware", "ls", "--json"]]
        assert app._data_dir == fake_cli.data_dir
        # every data command carried the data directory, in the equals form
        assert all(c["args"][0] == "--no-usb-reset" and c["args"][1] == f"--data-dir={fake_cli.data_dir}"
                   for c in fake_cli.calls()[1:])
        assert dialogs.shown == []

    def test_cameras_come_from_the_listing_keyed_by_model_and_serial(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        keys = [(c["model"], c["serial"]) for c in app.cameras]
        assert ("Z 9", "id-1") in keys and ("Z6_3", "id-1") in keys     # the shared id-N serial
        assert app.selected == 0
        assert len(app._cam_lb.get(0, "end")) == len(app.cameras)

    def test_the_listing_shows_the_selected_cameras_rows_and_flags_unreadable_files(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        cam = app.cameras[app.selected]
        iids = app._tree.get_children()
        shown = {i for i in iids if "unreadable" not in app._tree.item(i, "tags")}
        want = {r.file for r in app._listing.rows if (r.model, r.serial) == (cam["model"], cam["serial"])}
        assert shown == want
        assert any("unreadable" in app._tree.item(i, "tags") for i in iids)       # broken.json
        assert "unreadable" in app._snap_note.get()

    def test_a_reference_row_is_marked(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        idx = next(i for i, c in enumerate(app.cameras) if (c["model"], c["serial"]) == ("Z6_3", "id-1"))
        app._cam_lb.selection_clear(0, "end")
        app._cam_lb.selection_set(idx)
        app._on_cam_select(None)
        assert app._tree.set("id1_z6.json", "ref") == "◀ ref"

    def test_firmware_rows_show_a_missing_binary_and_unreadable_entries(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        statuses = sorted(app._fw_tree.set(i, "status") for i in app._fw_tree.get_children())
        assert "BINARY MISSING" in statuses and "unreadable: metadata_corrupt" in statuses
        assert "unreadable" in app._fw_note.get()

    def test_a_settings_error_is_shown_and_nothing_is_guessed(self, tk_root, fake_cli, dialogs):
        fake_cli.spec["settings show"] = {"rc": 1, "stderr": "Error: /s.json is not valid JSON; it was left as it is"}
        fake_cli.write()
        app = fleet_gui.FleetApp(tk_root)
        pump(tk_root, lambda: bool(dialogs.shown))
        settle(tk_root, app)
        assert app._data_dir is None
        assert dialogs.shown[0][0] == "showerror" and "not valid JSON" in dialogs.shown[0][2]
        assert fake_cli.commands() == [["settings", "show", "--json"]]      # no listing without a data dir

    def test_a_listing_that_is_not_json_is_an_error_not_an_empty_list(self, tk_root, fake_cli, dialogs):
        fake_cli.spec["ls"] = {"stdout": "InitializeSDK Execution duration: 9\n{}"}
        fake_cli.write()
        app = fleet_gui.FleetApp(tk_root)
        pump(tk_root, lambda: bool(dialogs.shown))
        settle(tk_root, app)
        assert "not a single JSON document" in dialogs.shown[0][2]
        assert app.cameras == []


class TestNoCli:
    def test_a_missing_binary_is_an_error_message_not_a_crash(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        monkeypatch.setenv(ENV_CLI, str(tmp_path / "does-not-exist"))
        app = fleet_gui.FleetApp(tk_root)
        tk_root.update()
        assert dialogs.shown[0][:2] == ("showerror", "CLI not found")
        assert ENV_CLI in dialogs.shown[0][2]
        assert app._data_dir is None


class TestDiscover:
    def test_zero_cameras_clears_the_selection_and_the_guards_hold(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        assert app.selected is not None
        fake_cli.spec["discover"] = {"stdout": '{"cameras": []}'}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert app.cameras == [] and app.selected is None and app._cam_lb.size() == 0
        assert app._selected_camera() is None and app._selected_serial() is None
        before = len(fake_cli.calls())
        app.take_snapshot()                       # used to raise IndexError in the Tk callback
        assert dialogs.shown[-1][:2] == ("showwarning", "No camera")
        assert len(fake_cli.calls()) == before

    def test_a_stale_selection_index_is_not_a_camera(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        app.selected = len(app.cameras) + 3
        assert app._selected_camera() is None
        app.selected = -1
        assert app._selected_camera() is None

    def test_discovered_cameras_replace_the_known_ones_and_keep_the_selection_if_present(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["discover"] = {"stdout": _fixture("discover.json")}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert [c["model"] for c in app.cameras] == ["Z 9", "Z6_3", "Z 5"]
        assert app.selected == 0
        assert "no capture baseline" in notes(app)       # the Z 5

    def test_the_selection_follows_its_camera_by_model_and_serial_not_by_position(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        idx = next(i for i, c in enumerate(app.cameras) if (c["model"], c["serial"]) == ("Z6_3", "id-1"))
        assert idx != 0
        app._cam_lb.selection_clear(0, "end")
        app._cam_lb.selection_set(idx)
        app._on_cam_select(None)
        cams = [{"model": "Z 9", "serial": "id-1", "firmware": "5"}, {"model": "Z6_3", "serial": "id-1", "firmware": "2"}]
        fake_cli.spec["discover"] = {"stdout": json.dumps({"cameras": cams})}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert app.selected == 1 and app.cameras[app.selected]["model"] == "Z6_3"   # not index 0: same serial, other model

    def test_a_bad_discover_document_leaves_the_list_as_it_was(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        before = list(app.cameras)
        fake_cli.spec["discover"] = {"stdout": '{"cameras": [{"model": "Z 9", "serial": "S"}, {"model": "Z 5"}]}'}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert app.cameras == before
        assert dialogs.shown[-1][0] == "showerror"
        assert app._cam_lb.size() == len(before)             # listbox and list stay in step

    def test_a_discover_failure_is_shown_with_the_cli_message(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["discover"] = {"rc": 1, "stderr": "Error: no Nikon cameras detected"}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert dialogs.shown[-1] == ("showerror", "Discover failed", "Error: no Nikon cameras detected")

    def test_warnings_from_a_successful_run_reach_the_user(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["discover"] = {"stdout": '{"cameras": []}', "stderr": "warning: skipped 1 body (no serial)\n"}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert "skipped 1 body" in notes(app)


class TestOneCallAtATime:
    def test_buttons_are_disabled_while_a_call_is_in_flight_and_a_second_start_is_refused(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["discover"] = {"stdout": '{"cameras": []}', "sleep": 1.0}
        fake_cli.write()
        app.discover()
        tk_root.update()
        assert app._cli.busy
        assert app._busy_widgets and all(str(w.cget("state")) == "disabled" for w in app._busy_widgets)
        assert app._call(["ls", "--json"], lambda r: None) is False           # refused, not queued
        app.discover()                                                       # a double click
        settle(tk_root, app)
        assert [c for c in fake_cli.commands() if c[0] == "discover"] == [["discover", "--json"]]
        assert all(str(w.cget("state")) == "normal" for w in app._busy_widgets)

    def test_a_hung_cli_is_killed_and_reported_and_the_app_is_usable_again(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["discover"] = {"stdout": "", "sleep": 60}
        fake_cli.write()
        monkeypatch.setattr(fleet_gui, "TIMEOUT_CAMERA_S", 0.5)
        app.discover()
        settle(tk_root, app)
        assert dialogs.shown[-1][0] == "showerror" and "timed out" in dialogs.shown[-1][2]
        assert not app._cli.busy
        assert all(str(w.cget("state")) == "normal" for w in app._busy_widgets)

    def test_the_ui_thread_keeps_running_while_the_cli_is_slow(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["discover"] = {"stdout": '{"cameras": []}', "sleep": 0.6}
        fake_cli.write()
        ticks = []
        t0 = time.monotonic()
        app.discover()                               # returns at once; the call is on a worker thread
        assert time.monotonic() - t0 < 0.3
        tk_root.after(50, lambda: ticks.append(1))
        settle(tk_root, app)
        assert ticks == [1]                          # the event loop ran while the call was in flight


class TestSnapshots:
    def test_a_command_the_test_did_not_specify_fails_loudly(self, tk_root, fake_cli, dialogs):
        # The fake CLI is the test's only source of answers: a forgotten spec must show as a failure.
        app = make_app(tk_root, fake_cli)
        assert "snapshot" not in fake_cli.spec
        app.take_snapshot()
        settle(tk_root, app)
        assert dialogs.shown[-1][0] == "showerror" and "unspecified fake command: snapshot" in dialogs.shown[-1][2]
        assert "saved" not in app._status_var.get()

    def test_the_snapshot_argv_carries_serial_and_a_dashed_label_as_single_arguments(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        app._label.set("-guest")
        app.take_snapshot()
        settle(tk_root, app)
        snap = next(c for c in fake_cli.commands() if c[0] == "snapshot")
        cam = app.cameras[app.selected]
        assert snap == ["snapshot", f"--serial={cam['serial']}", "--label=-guest"]

    def test_a_refused_capture_shows_the_cli_message_and_does_not_report_success(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["snapshot"] = {"rc": 1, "stderr": "Error: capture incomplete (nothing written)"}
        fake_cli.write()
        app.take_snapshot()
        settle(tk_root, app)
        assert dialogs.shown[-1] == ("showerror", "Snapshot failed", "Error: capture incomplete (nothing written)")
        assert "saved" not in app._status_var.get()

    def test_a_good_capture_refreshes_the_listing(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        before = [c for c in fake_cli.commands() if c == ["ls", "--json"]]
        fake_cli.spec["snapshot"] = {"stdout": "saved\n"}
        fake_cli.write()
        app.take_snapshot()
        settle(tk_root, app)
        assert len([c for c in fake_cli.commands() if c == ["ls", "--json"]]) == len(before) + 1
        assert app._status_var.get() == "Snapshot saved."


class TestReference:
    def _select(self, app, tk_root, model, serial, file):
        idx = next(i for i, c in enumerate(app.cameras) if (c["model"], c["serial"]) == (model, serial))
        app._cam_lb.selection_clear(0, "end")
        app._cam_lb.selection_set(idx)
        app._on_cam_select(None)
        app._tree.selection_set(file)

    def test_it_goes_through_ref_set_with_an_absolute_path(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        self._select(app, tk_root, "Z 9", "XYZ789", "xyz_no_reference.json")
        fake_cli.spec["ref set"] = {"stdout": "Set reference for Z 9 XYZ789\n"}
        fake_cli.write()
        app.set_reference()
        settle(tk_root, app)
        cmd = next(c for c in fake_cli.commands() if c[:2] == ["ref", "set"])
        assert cmd == ["ref", "set", "--", str(fake_cli.data_dir / "snapshots" / "xyz_no_reference.json")]
        assert "Set reference" in notes(app)

    def test_replacing_an_existing_reference_asks_first_and_no_means_no_call(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        self._select(app, tk_root, "Z 9", "ABC123", "abc_older.json")       # ABC123 already has a reference
        dialogs.yes = False
        app.set_reference()
        settle(tk_root, app)
        asked = [d for d in dialogs.shown if d[0] == "askyesno"]
        assert asked and "already has a reference" in asked[0][2] and "abc_ref_source.json" in asked[0][2]
        assert not any(c[:2] == ["ref", "set"] for c in fake_cli.commands())

    def test_replacing_after_yes_calls_the_cli_and_shows_what_it_replaced(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        self._select(app, tk_root, "Z 9", "ABC123", "abc_older.json")
        fake_cli.spec["ref set"] = {"stdout": "Set reference\n  replaced the previous reference (captured X, label -)\n"}
        fake_cli.write()
        app.set_reference()
        settle(tk_root, app)
        assert any(c[:2] == ["ref", "set"] for c in fake_cli.commands())
        assert "replaced the previous reference" in notes(app)

    def test_a_snapshot_that_already_is_the_reference_is_not_re_set(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        self._select(app, tk_root, "Z 9", "ABC123", "abc_ref_source.json")
        app.set_reference()
        settle(tk_root, app)
        assert dialogs.shown[-1][0] == "showinfo"
        assert not any(c[:2] == ["ref", "set"] for c in fake_cli.commands())

    def test_an_unreadable_row_cannot_be_made_a_reference(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        app._tree.selection_set("broken.json")
        app.set_reference()
        assert dialogs.shown[-1][:2] == ("showwarning", "Unreadable snapshot")
        assert not any(c[:2] == ["ref", "set"] for c in fake_cli.commands())

    def test_a_cli_refusal_is_shown(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        self._select(app, tk_root, "Z 9", "XYZ789", "xyz_no_reference.json")
        fake_cli.spec["ref set"] = {"rc": 1, "stderr": "Error: serial is not a plain file-name component"}
        fake_cli.write()
        app.set_reference()
        settle(tk_root, app)
        assert dialogs.shown[-1][0] == "showerror" and "not a plain" in dialogs.shown[-1][2]


class TestFirmware:
    def _add(self, app, tk_root, dialogs, monkeypatch, name="Z6_3_0200.bin"):
        monkeypatch.setattr(fleet_gui.filedialog, "askopenfilename", lambda **kw: f"/tmp/{name}")
        dialogs.strings[:] = ["Z6_3", "2.00"]
        app.add_firmware()
        settle(tk_root, app)

    def test_the_model_suggestion_keeps_the_underscore_of_a_known_model(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        self._add(app, tk_root, dialogs, monkeypatch)
        (_, _, suggested), (_, _, version) = dialogs.asked
        assert suggested == "Z6_3" and version == "2.00"          # not "Z6 3"

    def test_exit_status_three_asks_to_overwrite_and_then_forces(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["firmware add"] = [{"rc": 3, "stderr": "Error: firmware archive already exists"}, {"rc": 0}]
        fake_cli.write()
        self._add(app, tk_root, dialogs, monkeypatch)
        adds = [c for c in fake_cli.commands() if c[:2] == ["firmware", "add"]]
        assert adds == [["firmware", "add", "--model=Z6_3", "--version=2.00", "--", "/tmp/Z6_3_0200.bin"],
                        ["firmware", "add", "--model=Z6_3", "--version=2.00", "--force", "--", "/tmp/Z6_3_0200.bin"]]
        assert any(d[0] == "askyesno" and "Overwrite" in d[2] for d in dialogs.shown)

    def test_declining_the_overwrite_sends_nothing_more(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["firmware add"] = {"rc": 3, "stderr": "x"}
        fake_cli.write()
        dialogs.yes = False
        self._add(app, tk_root, dialogs, monkeypatch)
        assert len([c for c in fake_cli.commands() if c[:2] == ["firmware", "add"]]) == 1

    def test_the_prompt_keys_on_the_exit_status_not_on_the_message_text(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        # the old text-matching trigger, with an ordinary failure status
        fake_cli.spec["firmware add"] = {"rc": 1, "stderr": "Error: firmware archive already exists for Z6_3 2.00"}
        fake_cli.write()
        self._add(app, tk_root, dialogs, monkeypatch)
        assert not any(d[0] == "askyesno" for d in dialogs.shown)
        assert dialogs.shown[-1][0] == "showerror"

    def test_a_status_three_with_a_different_message_still_prompts(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["firmware add"] = [{"rc": 3, "stderr": "refused: different bytes"}, {"rc": 0}]
        fake_cli.write()
        self._add(app, tk_root, dialogs, monkeypatch)
        assert any(d[0] == "askyesno" for d in dialogs.shown)

    def test_remove_deletes_only_the_listed_entry_directory(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        root = fake_cli.data_dir / "firmware"
        entry = root / "Z6_3" / "2.00"
        entry.mkdir(parents=True)
        (entry / "firmware.bin").write_bytes(b"x")
        (entry / "metadata.json").write_text("{}")
        other = root / "Z_9" / "5.31"
        other.mkdir(parents=True)
        (other / "firmware.bin").write_bytes(b"y")
        listing = json.loads(_fixture("firmware_ls.json"))
        listing["dir"] = str(root)
        for e in listing["entries"]:
            e["directory"] = str(root / e["directory"].removeprefix("/data/firmware/"))
        fake_cli.spec["firmware ls"] = {"stdout": json.dumps(listing)}
        fake_cli.write()
        app._refresh_firmware()
        settle(tk_root, app)
        app._fw_tree.selection_set(str(entry))
        app.remove_firmware()
        settle(tk_root, app)
        assert not entry.exists() and (other / "firmware.bin").exists()

    def test_remove_refuses_a_directory_that_is_not_an_entry(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        victim = fake_cli.data_dir / "firmware" / "Z_9"
        (victim / "5.31").mkdir(parents=True)
        (victim / "5.31" / "firmware.bin").write_bytes(b"keep")
        listing = json.loads(_fixture("firmware_ls.json"))
        listing["dir"] = str(fake_cli.data_dir / "firmware")
        listing["entries"] = [{"status": "unreadable", "directory": str(victim), "error_kind": "metadata_corrupt",
                               "error": "x"}]
        fake_cli.spec["firmware ls"] = {"stdout": json.dumps(listing)}
        fake_cli.write()
        app._refresh_firmware()
        settle(tk_root, app)
        app._fw_tree.selection_set(str(victim))
        app.remove_firmware()
        assert (victim / "5.31" / "firmware.bin").read_bytes() == b"keep"
        assert dialogs.shown[-1][0] == "showerror" and "Remove failed" == dialogs.shown[-1][1]

    def test_download_runs_on_its_own_worker_and_reports_the_outcome(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["firmware browse"] = {"stdout": "archived Z 9 5.33\n", "sleep": 0.3}
        fake_cli.write()
        dialogs.strings[:] = ["-Z 9"]
        app.download_firmware()
        assert str(app._dl_button.cget("state")) == "disabled"
        pump(tk_root, lambda: not app._browse.busy)
        assert [c for c in fake_cli.commands() if c[:2] == ["firmware", "browse"]] == [
            ["firmware", "browse", "--", "-Z 9"]]
        assert dialogs.shown[-1][:2] == ("showinfo", "Download from Nikon")
        assert str(app._dl_button.cget("state")) == "normal"

    def test_a_download_that_times_out_is_reported_and_the_worker_ends(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["firmware browse"] = {"sleep": 60}
        fake_cli.write()
        monkeypatch.setattr(fleet_gui, "TIMEOUT_BROWSE_S", 0.5)
        dialogs.strings[:] = ["Z 9"]
        app.download_firmware()
        pump(tk_root, lambda: not app._browse.busy)
        assert dialogs.shown[-1][0] == "showerror" and "timed out" in dialogs.shown[-1][2]


class TestArchiveJobs:
    def test_an_exception_in_export_is_shown_and_the_app_is_not_stuck(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        monkeypatch.setattr(fleet_gui.filedialog, "asksaveasfilename", lambda **kw: "/tmp/x.zip")

        def boom(*a):
            raise RuntimeError("disk exploded")
        monkeypatch.setattr(fleet_gui, "export_archive", boom)
        app.export_data()
        settle(tk_root, app)
        assert dialogs.shown[-1][0] == "showerror" and "disk exploded" in dialogs.shown[-1][2]
        assert not app._cli.busy and not app._cli_poll._ticking

    def test_an_exception_in_import_is_shown_and_the_app_is_not_stuck(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        monkeypatch.setattr(fleet_gui.filedialog, "askopenfilename", lambda **kw: "/tmp/x.zip")

        def boom(*a):
            raise ValueError("bad archive")
        monkeypatch.setattr(fleet_gui, "import_archive", boom)
        app.import_data()
        settle(tk_root, app)
        assert dialogs.shown[-1][0] == "showerror" and "bad archive" in dialogs.shown[-1][2]
        assert not app._cli.busy

    def test_an_import_reports_every_count_and_refreshes(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        import zipfile
        app = make_app(tk_root, fake_cli)
        ok = (PROJECT / "tests/fixtures/snapshot_validity/snapshots/ok.json").read_bytes()
        z = tmp_path / "in.zip"
        with zipfile.ZipFile(z, "w") as zf:
            zf.writestr("snapshots/a.json", ok)
            zf.writestr("snapshots/bad.json", b"nope")
            zf.writestr("other/x", b"?")
        monkeypatch.setattr(fleet_gui.filedialog, "askopenfilename", lambda **kw: str(z))
        before = len([c for c in fake_cli.commands() if c == ["ls", "--json"]])
        app.import_data()
        settle(tk_root, app)
        text = dialogs.shown[-1][2]
        assert "Imported 1 snapshot(s)" in text and "rejected 2" in text and "skipped 0 already present" in text
        assert len([c for c in fake_cli.commands() if c == ["ls", "--json"]]) == before + 1
        assert (fake_cli.data_dir / "snapshots" / "a.json").exists()

    def test_an_export_reports_what_it_left_out(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        app = make_app(tk_root, fake_cli)
        (fake_cli.data_dir / "snapshots").mkdir()
        (fake_cli.data_dir / "snapshots" / "junk.json").write_text("not a snapshot")
        dest = tmp_path / "out.zip"
        monkeypatch.setattr(fleet_gui.filedialog, "asksaveasfilename", lambda **kw: str(dest))
        app.export_data()
        settle(tk_root, app)
        assert "Left out 1 item(s)" in dialogs.shown[-1][2] and "junk.json" in dialogs.shown[-1][2]
        assert dest.exists()


class TestSettingsChange:
    def test_saving_calls_set_data_dir_and_reloads_the_settings(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        before = len(fake_cli.calls())
        fake_cli.spec["settings set-data-dir"] = {"stdout": "data_dir: /new/place\n"}
        fake_cli.write()
        app.show_prefs()
        win = [w for w in tk_root.winfo_children() if w.winfo_class() == "Toplevel"][-1]
        entries = [c for c in win.winfo_children() if c.winfo_class() == "TEntry"]
        entries[0].delete(0, "end")
        entries[0].insert(0, "/new/place")
        save = next(f for f in win.winfo_children() if f.winfo_class() == "TFrame" and any(
            b.cget("text") == "Save" for b in f.winfo_children()))
        next(b for b in save.winfo_children() if b.cget("text") == "Save").invoke()
        settle(tk_root, app)
        cmds = fake_cli.commands()[before:]
        assert cmds[0] == ["settings", "set-data-dir", "--", "/new/place"]
        assert cmds[1] == ["settings", "show", "--json"]        # re-resolved by the CLI, not guessed

    def test_a_cli_refusal_keeps_the_window_open_and_is_shown(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["settings set-data-dir"] = {"rc": 1, "stderr": "Error: relative is not an absolute path"}
        fake_cli.write()
        app.show_prefs()
        win = [w for w in tk_root.winfo_children() if w.winfo_class() == "Toplevel"][-1]
        save = next(f for f in win.winfo_children() if f.winfo_class() == "TFrame" and any(
            b.cget("text") == "Save" for b in f.winfo_children()))
        next(b for b in save.winfo_children() if b.cget("text") == "Save").invoke()
        settle(tk_root, app)
        assert win.winfo_exists()
        assert dialogs.shown[-1][0] == "showerror" and "absolute" in dialogs.shown[-1][2]
        win.destroy()


class TestDetailWindow:
    def test_malformed_data_is_shown_as_missing_and_does_not_raise(self, tk_root):
        data = {"camera": {"model": "Z 9"}, "captured_at": "2026-01-01T00:00:00Z", "properties": {
            "kNkMAIDCapability_NoCode": {"value": 1},
            "kNkMAIDCapability_NoValue": {"code": 7},
            "kNkMAIDCapability_Junk": 5,
            "kNkMAIDCapability_EnumNoValues": {"code": 8, "value": {"value_index": 3, "elem_type": 7}},
            "kNkMAIDCapability_WB": {"code": 9, "value": {"value": 0.0, "value_index": 1224, "lower": 0.0,
                                                          "upper": 1224.0, "steps": 1225}},
            "kNkMAIDCapability_Past": {"code": 10, "value": {"value": 0.0, "value_index": 1225, "lower": 0.0,
                                                             "upper": 1224.0, "steps": 1225}},
        }}
        win = fleet_gui.SnapshotDetailWindow(tk_root, "x.json", data)
        rows = {r[0]: r for r in win._all_rows}
        assert rows["NoCode"][2] == "(missing)" and rows["NoValue"][1] == "(missing)"
        assert rows["Junk"][1].startswith("[malformed entry")
        assert "without a values list" in rows["EnumNoValues"][1]
        assert rows["WB"][1] == "1224.0"
        assert "past" in rows["Past"][1]
        win._tree.winfo_toplevel().destroy()


DISCOVERED = [
    {"model": "Z 9", "serial": "0000003023668", "firmware": "5.31"},
    {"model": "Z6_3", "serial": "id-2", "firmware": "2.00"},
]


class TestVendorOps:
    def _window(self, app, tk_root, fake_cli=None, cameras=None, select=0):
        """The vendor window. With *fake_cli* a Discover runs first, so the camera
        is one seen on USB in this session (writes are only offered for those)."""
        if fake_cli is not None:
            fake_cli.spec["discover"] = {"stdout": json.dumps({"cameras": cameras or DISCOVERED})}
            fake_cli.write()
            app.discover()
            settle(tk_root, app)
            app._cam_lb.selection_clear(0, "end")
            app._cam_lb.selection_set(select)
            app._on_cam_select(None)
        win = fleet_gui.VendorOpsWindow(tk_root, app)
        tk_root.update()
        return win

    def _fill(self, win, **over):
        vals = {"profile_name": "P", "ssid_24ghz": "s24", "ssid_5ghz": "s5", "host": "h", "port": "21",
                "username": "u", "password": "pw"} | over
        for k, v in vals.items():
            win._fields[k].set(v)

    def _out(self, box):
        return box.get("1.0", "end").strip()

    def _writes(self, fake_cli):
        return [c for c in fake_cli.calls() if c["args"] and "vendor-write-ftp" in c["args"]]

    def test_the_target_is_the_selected_cameras_serial_and_read_uses_the_equals_form(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root)
        fake_cli.spec["vendor-read"] = {"stdout": "0xd053: 2 byte(s): 01 02\n", "stderr": "warning: model unconfirmed\n"}
        fake_cli.write()
        win._do_read()
        settle(tk_root, app)
        cmd = next(c for c in fake_cli.commands() if c[0] == "vendor-read")
        assert cmd == ["vendor-read", "0xd053", f"--serial={app.cameras[0]['serial']}"]
        out = self._out(win._read_box)
        assert "01 02" in out and "model unconfirmed" in out     # the warning is shown with the answer
        win._window.destroy()

    def test_a_failed_read_shows_the_cli_text_once(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root)
        fake_cli.spec["vendor-read"] = {"rc": 1, "stderr": "Error: camera busy"}
        fake_cli.write()
        win._do_read()
        settle(tk_root, app)
        assert self._out(win._read_box) == "Error: camera busy"
        win._window.destroy()

    def test_an_invalid_property_code_sends_nothing(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root)
        win._code_var.set("0x1_0")
        before = len(fake_cli.calls())
        win._do_read()
        assert "Invalid property code" in self._out(win._read_box) and len(fake_cli.calls()) == before
        win._window.destroy()

    def test_an_invalid_port_sends_nothing(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root)
        before = len(fake_cli.calls())
        for bad in ("0", "65536", "+21", "2_1", ""):
            self._fill(win, port=bad)
            win._do_write(dry_run=True)
            assert "Invalid port" in self._out(win._write_box), bad
        assert len(fake_cli.calls()) == before
        win._window.destroy()

    def test_a_write_is_validated_by_a_dry_run_first_and_the_password_goes_on_stdin(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = [{"stdout": "Would write 283 byte(s)\n"}, {"stdout": "Wrote FTP profile\n"}]
        fake_cli.write()
        self._fill(win, profile_name="-guest", password="s3cret!")
        win._do_write(dry_run=False)
        settle(tk_root, app)
        calls = self._writes(fake_cli)
        assert len(calls) == 2
        assert "--dry-run" in calls[0]["args"] and "--dry-run" not in calls[1]["args"]
        assert all("--profile-name=-guest" in c["args"] and "--password-stdin" in c["args"] for c in calls)
        assert all(c["stdin"] == "s3cret!" for c in calls)
        assert not any("s3cret" in a for c in calls for a in c["args"])
        assert "Wrote FTP profile" in self._out(win._write_box)
        win._window.destroy()

    def test_values_reach_the_cli_exactly_as_typed(self, tk_root, fake_cli, dialogs):
        # `trimmed_for_required_check` is for the CLI's emptiness check; the GUI never alters a value.
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = [{"stdout": "Would write\n"}, {"stdout": "Wrote\n"}]
        fake_cli.write()
        self._fill(win, ssid_24ghz="home ", profile_name=" lead", password=" pw ")
        win._do_write(dry_run=False)
        settle(tk_root, app)
        for c in self._writes(fake_cli):
            assert "--ssid-24ghz=home " in c["args"] and "--profile-name= lead" in c["args"]
            assert c["stdin"] == " pw "
        win._window.destroy()

    def test_a_whitespace_only_field_is_left_for_the_cli_to_refuse(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = {"rc": 1, "stderr": "Error: missing field(s): SSID (2.4GHz)"}
        fake_cli.write()
        self._fill(win, ssid_24ghz="   ")
        win._do_write(dry_run=False)
        settle(tk_root, app)
        assert "--ssid-24ghz=   " in self._writes(fake_cli)[0]["args"]
        assert not any(d[0] == "askyesno" for d in dialogs.shown)
        assert "Not sent" in self._out(win._write_box)
        win._window.destroy()

    def test_a_refusal_in_the_dry_run_never_reaches_the_confirm_dialog_or_the_camera(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = {"rc": 1, "stderr": "Error: refusing to write an FTP profile with missing field(s): SSID (2.4GHz)"}
        fake_cli.write()
        self._fill(win, ssid_24ghz="")
        win._do_write(dry_run=False)
        settle(tk_root, app)
        calls = self._writes(fake_cli)
        assert len(calls) == 1 and "--dry-run" in calls[0]["args"]
        assert not any(d[0] == "askyesno" for d in dialogs.shown)
        assert "Not sent" in self._out(win._write_box) and "SSID (2.4GHz)" in self._out(win._write_box)
        win._window.destroy()

    def test_declining_the_confirmation_sends_nothing(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = {"stdout": "Would write\n"}
        fake_cli.write()
        dialogs.yes = False
        self._fill(win)
        win._do_write(dry_run=False)
        settle(tk_root, app)
        calls = self._writes(fake_cli)
        assert len(calls) == 1 and "Cancelled" in self._out(win._write_box)
        win._window.destroy()

    # which camera a write is aimed at

    def _confirm_text(self, dialogs):
        return next(d[2] for d in reversed(dialogs.shown) if d[0] == "askyesno")

    def test_the_confirm_dialog_names_the_camera_by_model_and_stored_serial(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = {"stdout": "ok\n"}
        fake_cli.write()
        self._fill(win, profile_name="Studio", host="ftp.example", username="bob")
        win._do_write(dry_run=False)
        settle(tk_root, app)
        text = self._confirm_text(dialogs)
        assert "Camera: Z 9  0000003023668" in text
        assert "Name: Studio" in text and "Host: ftp.example:21" in text and "User: bob" in text
        assert "no readable serial" not in text
        win._window.destroy()

    def test_an_id_n_serial_is_called_what_it_is(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli, select=1)
        fake_cli.spec["vendor-write-ftp"] = {"stdout": "ok\n"}
        fake_cli.write()
        self._fill(win)
        win._do_write(dry_run=False)
        settle(tk_root, app)
        text = self._confirm_text(dialogs)
        assert "Z6_3  id-2" in text and "the one attached body with no readable serial" in text
        assert "--serial=id-2" in next(c for c in self._writes(fake_cli) if "--dry-run" not in c["args"])["args"]
        win._window.destroy()

    def test_a_camera_known_only_from_snapshots_cannot_be_written_to(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)                # the list is from saved snapshots, no Discover yet
        assert app._cameras_are_known_only and app._selected_camera() is not None
        win = self._window(app, tk_root)
        buttons = [w for w in _all_widgets(win._window) if w.winfo_class() == "TButton"]
        write = next(b for b in buttons if b.cget("text").startswith("Write to Camera"))
        preview = next(b for b in buttons if b.cget("text").startswith("Preview"))
        assert str(write.cget("state")) == "disabled" and str(preview.cget("state")) != "disabled"
        reasons = [w.cget("text") for w in _all_widgets(win._window) if w.winfo_class() == "TLabel"]
        assert any("saved snapshots" in t and "Discover" in t for t in reasons)
        self._fill(win)
        win._do_write(dry_run=False)                     # even called directly
        settle(tk_root, app)
        assert self._writes(fake_cli) == []
        assert "Not sent" in self._out(win._write_box) and "Discover" in self._out(win._write_box)
        assert not any(d[0] == "askyesno" for d in dialogs.shown)
        win._window.destroy()

    def test_the_disabled_write_button_stays_disabled_after_a_call_finishes(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root)
        fake_cli.spec["vendor-read"] = {"stdout": "0xd053: 0 byte(s): \n"}
        fake_cli.write()
        win._do_read()
        settle(tk_root, app)
        write = next(b for b in _all_widgets(win._window) if b.winfo_class() == "TButton"
                     and b.cget("text").startswith("Write to Camera"))
        assert str(write.cget("state")) == "disabled"
        win._window.destroy()

    def test_the_preview_is_still_available_for_a_snapshot_only_camera(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root)
        fake_cli.spec["vendor-write-ftp"] = {"stdout": "Would write\n"}
        fake_cli.write()
        self._fill(win)
        win._do_write(dry_run=True)
        settle(tk_root, app)
        assert len(self._writes(fake_cli)) == 1 and "Would write" in self._out(win._write_box)
        win._window.destroy()

    def test_no_selected_camera_means_auto_detect_and_the_dialog_says_so(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        app.selected = None
        win = self._window(app, tk_root)
        fake_cli.spec["vendor-write-ftp"] = {"stdout": "ok\n"}
        fake_cli.write()
        self._fill(win)
        win._do_write(dry_run=False)
        settle(tk_root, app)
        assert "the only Nikon camera on USB (auto-detected)" in self._confirm_text(dialogs)
        assert not any("--serial" in a for c in self._writes(fake_cli) for a in c["args"])
        win._window.destroy()

    # how a failed real write is described

    def _real_write(self, tk_root, app, fake_cli, dialogs, second):
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = [{"stdout": "Would write\n"}, second]
        fake_cli.write()
        self._fill(win)
        win._do_write(dry_run=False)
        settle(tk_root, app)
        out = self._out(win._write_box)
        win._window.destroy()
        return out

    def test_exit_4_says_the_camera_may_have_changed(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        out = self._real_write(tk_root, app, fake_cli, dialogs,
                               {"rc": 4, "stderr": "Error: FTP write may have been applied: reply timed out"})
        assert "reply timed out" in out
        assert "camera may have changed: check the FTP profile in the camera's menu" in out

    def test_exit_1_is_a_plain_failure_and_its_text_is_shown_once(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        out = self._real_write(tk_root, app, fake_cli, dialogs, {"rc": 1, "stderr": "Error: camera busy"})
        assert out == "Error: camera busy"

    def test_exit_2_is_a_plain_failure(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        out = self._real_write(tk_root, app, fake_cli, dialogs, {"rc": 2, "stderr": "error: unexpected argument"})
        assert "may have changed" not in out

    @pytest.mark.parametrize("rc", [101, 139, 3, 255])
    def test_any_other_exit_status_may_have_changed_the_camera(self, tk_root, fake_cli, dialogs, rc):
        app = make_app(tk_root, fake_cli)
        out = self._real_write(tk_root, app, fake_cli, dialogs, {"rc": rc, "stderr": "boom"})
        assert "boom" in out and "may have changed" in out

    def test_a_write_killed_by_a_signal_may_have_changed_the_camera(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        out = self._real_write(tk_root, app, fake_cli, dialogs, {"signal": 9, "stderr": "partial output"})
        assert "partial output" in out and "may have changed" in out

    def test_success_shows_the_stderr_note_about_a_mismatched_reply(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        out = self._real_write(tk_root, app, fake_cli, dialogs,
                               {"rc": 0, "stdout": "Wrote FTP profile\n",
                                "stderr": "note: the reply did not match the command\n"})
        assert "Wrote FTP profile" in out and "did not match the command" in out
        assert "may have changed" not in out
        assert "did not match the command" in notes(app)      # also kept in the Messages box

    def test_a_write_killed_on_the_timeout_may_have_sent_the_data(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        monkeypatch.setattr(fleet_gui, "TIMEOUT_CAMERA_S", 0.5)
        out = self._real_write(tk_root, app, fake_cli, dialogs, {"sleep": 60})
        assert "timed out" in out and "may have changed" in out

    def test_a_dry_run_button_never_sends_the_real_write(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-write-ftp"] = {"stdout": "Would write\n"}
        fake_cli.write()
        self._fill(win)
        win._do_write(dry_run=True)
        settle(tk_root, app)
        calls = self._writes(fake_cli)
        assert len(calls) == 1 and "--dry-run" in calls[0]["args"]
        assert not any(d[0] == "askyesno" for d in dialogs.shown)
        win._window.destroy()

    def test_the_window_buttons_are_disabled_while_a_call_is_in_flight(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        win = self._window(app, tk_root, fake_cli)
        fake_cli.spec["vendor-read"] = {"stdout": "0xd053: 0 byte(s): \n", "sleep": 0.8}
        fake_cli.write()
        win._do_read()
        tk_root.update()
        buttons = [w for w in app._busy_widgets if str(w.winfo_toplevel()) == str(win._window)]
        assert len(buttons) == 3 and all(str(b.cget("state")) == "disabled" for b in buttons)
        settle(tk_root, app)
        assert all(str(b.cget("state")) == "normal" for b in buttons)
        win._window.destroy()


def _all_widgets(widget):
    for child in widget.winfo_children():
        yield child
        yield from _all_widgets(child)


class _StubRoot:
    def __init__(self):
        self.scheduled = []

    def after(self, ms, fn):
        self.scheduled.append(fn)


class TestPoller:
    def test_a_callback_whose_error_handler_raises_does_not_stop_the_polling_machinery(self):
        from fleet_cli import CliResult, JobRunner
        root = _StubRoot()

        def bad_handler(_e):
            raise RuntimeError("the error handler itself failed")
        runner = JobRunner(on_callback_error=bad_handler)
        poller = fleet_gui._Poller(root, runner)
        runner.submit(lambda: CliResult(0), lambda r: 1 / 0)
        poller.start()
        deadline = time.monotonic() + 5
        while root.scheduled and time.monotonic() < deadline:
            tick = root.scheduled.pop(0)
            try:
                tick()
            except RuntimeError:
                pass                      # Tk would report this one; the point is the state afterwards
            time.sleep(0.005)
        assert not runner.busy and poller._ticking is False
        # and a new job is polled again
        got = []
        runner.submit(lambda: CliResult(0, "again"), got.append)
        poller.start()
        while root.scheduled and time.monotonic() < deadline:
            root.scheduled.pop(0)()
            time.sleep(0.005)
        assert [r.stdout for r in got] == ["again"]

    def test_polling_is_rescheduled_while_a_job_is_in_flight_even_if_poll_raises(self):
        root = _StubRoot()

        class Runner:
            busy = True

            def poll(self):
                raise ValueError("boom")
        poller = fleet_gui._Poller(root, Runner())
        poller.start()
        with pytest.raises(ValueError):
            root.scheduled.pop(0)()
        assert len(root.scheduled) == 1 and poller._ticking is True


class TestStaleBinary:
    def test_an_old_binary_gets_a_visible_warning_once_and_still_runs(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        project = tmp_path / "proj"
        (project / "src").mkdir(parents=True)
        newer = project / "src" / "main.rs"
        newer.write_text("//")
        import os
        script = os.environ[ENV_CLI]
        os.utime(script, (1000, 1000))
        os.utime(newer, (2000, 2000))
        monkeypatch.delenv(ENV_CLI)
        monkeypatch.setattr(fleet_gui, "_PROJECT", project)
        monkeypatch.setattr(fleet_gui, "find_cli", lambda p: __import__("pathlib").Path(script))
        app = make_app(tk_root, fake_cli)
        assert notes(app).count("older than the source: rebuild with cargo build") == 1      # once, not per call
        assert app._data_dir == fake_cli.data_dir                                           # and it ran

    def test_a_fresh_binary_is_not_warned_about(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        project = tmp_path / "proj"
        (project / "src").mkdir(parents=True)
        (project / "src" / "main.rs").write_text("//")
        import os
        script = os.environ[ENV_CLI]
        os.utime(project / "src" / "main.rs", (1000, 1000))
        os.utime(script, (2000, 2000))
        monkeypatch.delenv(ENV_CLI)
        monkeypatch.setattr(fleet_gui, "_PROJECT", project)
        monkeypatch.setattr(fleet_gui, "find_cli", lambda p: __import__("pathlib").Path(script))
        app = make_app(tk_root, fake_cli)
        assert "older than the source" not in notes(app)

    def test_an_explicit_override_is_never_second_guessed(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        project = tmp_path / "proj"
        (project / "src").mkdir(parents=True)
        (project / "src" / "main.rs").write_text("//")
        import os
        os.utime(os.environ[ENV_CLI], (1000, 1000))
        os.utime(project / "src" / "main.rs", (2000, 2000))
        monkeypatch.setattr(fleet_gui, "_PROJECT", project)
        app = make_app(tk_root, fake_cli)
        assert "older than the source" not in notes(app)


class TestFirmwareRemove:
    def _listing(self, fake_cli, root, entries):
        listing = {"dir": str(root), "dir_exists": True, "skipped_dir_entries": 0, "entries": [
            {"status": "ok", "model": m, "version": v, "directory": str(root / slug / v), "bin_present": True,
             "bin_sha256": "0" * 64, "bin_size_bytes": 1, "archived_at": "t", "archived_by": "x",
             "canonical_snapshot_path": None, "notes": None, "provenance": None} for slug, m, v in entries]}
        fake_cli.spec["firmware ls"] = {"stdout": json.dumps(listing)}
        fake_cli.write()

    def _make_entry(self, root, slug, version):
        d = root / slug / version
        d.mkdir(parents=True)
        (d / "firmware.bin").write_bytes(b"x")
        (d / "metadata.json").write_text("{}")
        return d

    def test_declining_the_confirmation_deletes_nothing(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        root = fake_cli.data_dir / "firmware"
        entry = self._make_entry(root, "Z_9", "5.31")
        self._listing(fake_cli, root, [("Z_9", "Z 9", "5.31")])
        app._refresh_firmware()
        settle(tk_root, app)
        app._fw_tree.selection_set(str(entry))
        dialogs.yes = False
        app.remove_firmware()
        assert (entry / "firmware.bin").exists() and (entry / "metadata.json").exists()
        assert any(d[0] == "askyesno" and "Remove" in d[1] for d in dialogs.shown)
        assert app._archive_op is None

    def test_a_listing_that_reports_a_foreign_directory_cannot_make_the_gui_delete_there(self, tk_root, fake_cli, dialogs, tmp_path):
        app = make_app(tk_root, fake_cli)
        foreign = tmp_path / "somewhere else" / "firmware"
        entry = self._make_entry(foreign, "Z_9", "5.31")
        self._listing(fake_cli, foreign, [("Z_9", "Z 9", "5.31")])
        app._refresh_firmware()
        settle(tk_root, app)
        app._fw_tree.selection_set(str(entry))
        app.remove_firmware()
        assert (entry / "firmware.bin").exists() and (entry / "metadata.json").exists()
        assert dialogs.shown[-1][0] == "showerror" and "not under the firmware directory" in dialogs.shown[-1][2]
        assert str(fake_cli.data_dir / "firmware") in dialogs.shown[-1][2]

    def test_with_no_known_data_directory_nothing_is_removed(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        root = fake_cli.data_dir / "firmware"
        entry = self._make_entry(root, "Z_9", "5.31")
        self._listing(fake_cli, root, [("Z_9", "Z 9", "5.31")])
        app._refresh_firmware()
        settle(tk_root, app)
        app._fw_tree.selection_set(str(entry))
        app._data_dir = None
        app.remove_firmware()
        assert (entry / "firmware.bin").exists()
        assert dialogs.shown[-1][0] == "showerror"


class TestOneArchiveWriterAtATime:
    def _select_entry(self, app, tk_root, fake_cli):
        root = fake_cli.data_dir / "firmware"
        d = root / "Z_9" / "5.31"
        d.mkdir(parents=True)
        (d / "firmware.bin").write_bytes(b"x")
        listing = {"dir": str(root), "dir_exists": True, "skipped_dir_entries": 0, "entries": [
            {"status": "ok", "model": "Z 9", "version": "5.31", "directory": str(d), "bin_present": True,
             "bin_sha256": "0" * 64, "bin_size_bytes": 1, "archived_at": "t", "archived_by": "x",
             "canonical_snapshot_path": None, "notes": None, "provenance": None}]}
        fake_cli.spec["firmware ls"] = {"stdout": json.dumps(listing)}
        fake_cli.write()
        app._refresh_firmware()
        settle(tk_root, app)
        app._fw_tree.selection_set(str(d))
        return d

    def test_while_a_download_runs_import_remove_and_add_are_refused(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        app = make_app(tk_root, fake_cli)
        entry = self._select_entry(app, tk_root, fake_cli)
        fake_cli.spec["firmware browse"] = {"stdout": "archived\n", "sleep": 1.5}
        fake_cli.write()
        dialogs.strings[:] = ["Z 9"]
        app.download_firmware()
        assert app._archive_op == "download"
        imported = []
        monkeypatch.setattr(fleet_gui, "import_archive", lambda *a: imported.append(a))
        monkeypatch.setattr(fleet_gui.filedialog, "askopenfilename", lambda **kw: str(tmp_path / "x.zip"))
        app.import_data()
        app.remove_firmware()
        dialogs.strings[:] = ["Z 9", "5.31"]
        app.add_firmware()
        assert imported == []
        assert (entry / "firmware.bin").exists()
        assert not any(c[:2] == ["firmware", "add"] for c in fake_cli.commands())
        assert "download" in app._status_var.get()
        pump(tk_root, lambda: not app._browse.busy)
        settle(tk_root, app)
        assert app._archive_op is None

    def test_while_an_import_runs_a_download_is_refused(self, tk_root, fake_cli, dialogs, monkeypatch, tmp_path):
        import zipfile
        app = make_app(tk_root, fake_cli)
        z = tmp_path / "in.zip"
        with zipfile.ZipFile(z, "w"):
            pass
        monkeypatch.setattr(fleet_gui.filedialog, "askopenfilename", lambda **kw: str(z))
        real = fleet_gui.import_archive

        def slow(src, dd):
            time.sleep(1.0)
            return real(src, dd)
        monkeypatch.setattr(fleet_gui, "import_archive", slow)
        app.import_data()
        assert app._archive_op == "import"
        dialogs.strings[:] = ["Z 9"]
        app.download_firmware()
        assert not any(c[:2] == ["firmware", "browse"] for c in fake_cli.commands())
        assert not app._browse.busy
        settle(tk_root, app)
        assert app._archive_op is None

    def test_the_slot_is_released_when_an_add_is_declined_failed_or_done(self, tk_root, fake_cli, dialogs, monkeypatch):
        app = make_app(tk_root, fake_cli)
        monkeypatch.setattr(fleet_gui.filedialog, "askopenfilename", lambda **kw: "/tmp/Z_9_0531.bin")
        for spec, yes in (({"rc": 0}, True), ({"rc": 1, "stderr": "Error: x"}, True),
                          ({"rc": 3}, False), ([{"rc": 3}, {"rc": 0}], True), ([{"rc": 3}, {"rc": 1}], True)):
            fake_cli.spec["firmware add"] = spec
            fake_cli.write()
            dialogs.yes = yes
            dialogs.strings[:] = ["Z 9", "5.31"]
            app.add_firmware()
            settle(tk_root, app)
            assert app._archive_op is None, spec

    def test_a_download_refused_with_exit_3_says_what_it_means(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["firmware browse"] = {"rc": 3, "stderr": "Error: Z 9 5.33 is already archived with different bytes"}
        fake_cli.write()
        dialogs.strings[:] = ["Z 9"]
        app.download_firmware()
        pump(tk_root, lambda: not app._browse.busy)
        assert dialogs.shown[-1][0] == "showerror"
        text = dialogs.shown[-1][2]
        assert "already archived with different bytes" in text and "different contents" in text
        assert "Remove the existing entry first" in text
        assert not any(d[0] == "askyesno" for d in dialogs.shown)       # browse has no --force: nothing to confirm
        assert app._archive_op is None


class TestSmallGuards:
    def test_set_reference_without_a_data_directory_is_an_error_not_a_type_error(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        idx = next(i for i, c in enumerate(app.cameras) if (c["model"], c["serial"]) == ("Z 9", "XYZ789"))
        app._cam_lb.selection_clear(0, "end")
        app._cam_lb.selection_set(idx)
        app._on_cam_select(None)
        app._tree.selection_set("xyz_no_reference.json")
        app._data_dir = None
        app.set_reference()
        assert dialogs.shown[-1][0] == "showerror" and "data directory is not known" in dialogs.shown[-1][2]
        assert not any(c[:2] == ["ref", "set"] for c in fake_cli.commands())

    def test_a_selection_that_is_not_in_the_new_list_falls_back_to_the_first_camera(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        idx = next(i for i, c in enumerate(app.cameras) if (c["model"], c["serial"]) == ("Z6_3", "id-1"))
        app._cam_lb.selection_clear(0, "end")
        app._cam_lb.selection_set(idx)
        app._on_cam_select(None)
        assert idx != 0
        fake_cli.spec["discover"] = {"stdout": json.dumps({"cameras": DISCOVERED})}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert app.selected == 0 and app.cameras[0]["model"] == "Z 9"
        assert app._cam_lb.curselection() == (0,)

    def test_an_error_whose_text_already_says_error_is_not_prefixed_again(self, tk_root, fake_cli, dialogs):
        app = make_app(tk_root, fake_cli)
        fake_cli.spec["discover"] = {"rc": 1, "stderr": "Error: no Nikon cameras detected"}
        fake_cli.write()
        app.discover()
        settle(tk_root, app)
        assert app._status_var.get() == "Error: no Nikon cameras detected"


class TestMessagesBoxIsResizable:
    def test_the_messages_box_and_the_main_area_share_a_vertical_paned_window(self, tk_root, fake_cli):
        app = make_app(tk_root, fake_cli)
        app.root.update_idletasks()
        notes_frame = app._notes.master
        paned = notes_frame.master
        assert str(paned.cget("orient")) == "vertical", "the divider above Messages must be draggable"
        panes = [str(p) for p in paned.panes()]
        assert str(notes_frame) in panes and len(panes) == 2, panes
        # The main area is the other pane, and it holds the camera list.
        main = paned.nametowidget(next(p for p in panes if p != str(notes_frame)))
        assert str(app._cam_lb).startswith(str(main) + "."), (str(app._cam_lb), str(main))

    def test_dragging_the_divider_gives_the_messages_more_room(self, tk_root, fake_cli):
        app = make_app(tk_root, fake_cli)
        app.root.geometry("900x600")
        app.root.update()
        paned = app._notes.master.master
        before = app._notes.master.winfo_height()
        sash_y = paned.sashpos(0)
        paned.sashpos(0, sash_y - 80)
        app.root.update()
        after = app._notes.master.winfo_height()
        assert after >= before + 60, (before, after)
