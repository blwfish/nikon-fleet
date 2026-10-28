#!/usr/bin/env python3
"""Fleet GUI — Nikon camera fleet manager (tkinter / subprocess frontend).

A front end to the `nikon-fleet` CLI: what is listed (snapshots, firmware,
settings) is what the CLI's `--json` modes say, what is changed is changed by
the CLI. Every CLI call runs on a worker thread with a timeout (fleet_cli.py),
one at a time, and the buttons are disabled while one is in flight. The logic
that can be tested without a display is in fleet_lib.py, fleet_cli.py and
fleet_archive.py; this file is widgets and wiring.
"""

import os
import sys
import traceback
from pathlib import Path
import tkinter as tk
from tkinter import ttk, messagebox, filedialog, simpledialog

from fleet_archive import export_archive, import_archive, remove_firmware_entry
from fleet_cli import (
    ENV_CLI, CliNotFound, CliResult, JobRunner, binary_stale_warning, find_cli, run_cli,
    TIMEOUT_BROWSE_S, TIMEOUT_CAMERA_S, TIMEOUT_FIRMWARE_ADD_S, TIMEOUT_QUICK_S,
)
from fleet_lib import (
    EXIT_ALREADY_ARCHIVED, FTP_PASSWORD_KEY, is_id_fallback, FTP_TEXT_FIELDS, CliOutputError, ftp_write_failure_text,
    browse_outcome, camera_key, camera_label, discover_argv, firmware_add_argv, ftp_form_values,
    firmware_browse_argv, firmware_display_rows, firmware_ls_argv, global_argv,
    join_output, known_cameras, listing_notes, listing_ref_marks, ls_argv,
    parse_discover, parse_firmware_listing, parse_fw_filename, parse_listing,
    parse_propcode, parse_settings_show, property_rows,
    read_snapshot_file, ref_set_argv, rows_for_camera, set_data_dir_argv,
    settings_show_argv, snapshot_argv, suggest_fw_model, vendor_read_argv,
    vendor_write_ftp_argv,
)

_PROJECT = Path(__file__).resolve().parent.parent   # repo root

_POLL_MS = 100


class _Poller:
    """Calls a JobRunner's poll() from the Tk loop while a job is in flight."""

    def __init__(self, root: tk.Tk, runner: JobRunner) -> None:
        self._root = root
        self._runner = runner
        self._ticking = False

    def start(self) -> None:
        if not self._ticking:
            self._ticking = True
            self._root.after(_POLL_MS, self._tick)

    def _tick(self) -> None:
        try:
            self._runner.poll()
        finally:
            # Whatever a callback did (even raising out of its error handler),
            # keep polling while a job is in flight and never leave it half-stopped.
            if self._runner.busy:
                self._root.after(_POLL_MS, self._tick)
            else:
                self._ticking = False


# ── App ────────────────────────────────────────────────────────────────────

class FleetApp:
    def __init__(self, root: tk.Tk) -> None:
        self.root = root
        self.root.title("Fleet — Nikon Camera Manager")
        self.root.geometry("860x580")
        self.cameras: list[dict] = []
        self.selected: int | None = None
        self._data_dir: Path | None = None   # from `settings show`, never guessed
        self._listing = None                 # the last `ls --json`
        self._fw_listing = None              # the last `firmware ls --json`
        self._cameras_are_known_only = True  # the list is from snapshots, not a Discover
        self._busy_widgets: list = []
        self._archive_op: str | None = None  # the one operation that writes the firmware archive / data dir
        self._stale_checked = False
        self._cli = JobRunner(on_callback_error=self._callback_failed)
        self._cli_poll = _Poller(root, self._cli)
        self._browse = JobRunner(on_callback_error=self._callback_failed)
        self._browse_poll = _Poller(root, self._browse)
        self.root.report_callback_exception = self._tk_exception
        self._build_ui()
        self._status("Reading settings…")
        self._load_settings(then=self._refresh_all)
        self.root.after(100, self.root.focus_force)

    # ── UI ──────────────────────────────────────────────────────────────

    def _busy_button(self, parent, **kw) -> ttk.Button:
        """A button that is disabled while a CLI call is in flight."""
        b = ttk.Button(parent, **kw)
        self._register_busy(b)
        return b

    def _register_busy(self, widget) -> None:
        self._busy_widgets.append(widget)
        if self._cli.busy:
            widget.configure(state=tk.DISABLED)

    def _build_ui(self) -> None:
        bar = ttk.Frame(self.root, padding=4)
        bar.pack(fill=tk.X)
        self._busy_button(bar, text="⟳  Discover", command=self.discover).pack(side=tk.LEFT)
        ttk.Separator(bar, orient=tk.VERTICAL).pack(side=tk.LEFT, fill=tk.Y, padx=4)
        ttk.Label(bar, text="Label:").pack(side=tk.LEFT)
        self._label = tk.StringVar()
        self._label_entry = tk.Entry(bar, textvariable=self._label, width=18)
        self._label_entry.pack(side=tk.LEFT, padx=2)
        self._busy_button(bar, text="Take Snapshot", command=self.take_snapshot).pack(side=tk.LEFT, padx=2)
        ttk.Separator(bar, orient=tk.VERTICAL).pack(side=tk.LEFT, fill=tk.Y, padx=4)
        self._busy_button(bar, text="🔧  Vendor Ops", command=self.vendor_ops).pack(side=tk.LEFT)
        ttk.Separator(bar, orient=tk.VERTICAL).pack(side=tk.LEFT, fill=tk.Y, padx=4)
        ttk.Button(bar, text="⚙  Preferences", command=self.show_prefs).pack(side=tk.LEFT)
        self._status_var = tk.StringVar()
        ttk.Label(bar, textvariable=self._status_var, foreground="gray").pack(side=tk.LEFT, padx=8)

        # What the CLI said besides its answer (warnings on a successful run)
        # and what went wrong: kept, not just flashed in the status line.
        # The main area and the Messages box share a vertical PanedWindow, so the divider between
        # them can be dragged to give the messages more (or less) room.
        vpw = ttk.PanedWindow(self.root, orient=tk.VERTICAL)
        vpw.pack(fill=tk.BOTH, expand=True, padx=4, pady=4)

        pw = ttk.PanedWindow(vpw, orient=tk.HORIZONTAL)
        vpw.add(pw, weight=4)

        notes_f = ttk.LabelFrame(vpw, text="Messages", padding=2)
        self._notes = tk.Text(notes_f, height=4, wrap=tk.WORD, state=tk.DISABLED,
                              background="#f5f5f5", font=("Menlo", 10))
        notes_sb = ttk.Scrollbar(notes_f, orient=tk.VERTICAL, command=self._notes.yview)
        self._notes.configure(yscrollcommand=notes_sb.set)
        notes_sb.pack(side=tk.RIGHT, fill=tk.Y)
        self._notes.pack(fill=tk.BOTH, expand=True)
        vpw.add(notes_f, weight=1)

        # Camera list
        cam_f = ttk.LabelFrame(pw, text="Cameras", padding=4)
        self._cam_lb = tk.Listbox(cam_f, width=26, activestyle="none", exportselection=False,
                                   selectbackground="#4a9eff", selectforeground="white")
        self._cam_lb.pack(fill=tk.BOTH, expand=True)
        self._cam_lb.bind("<<ListboxSelect>>", self._on_cam_select)
        pw.add(cam_f, weight=1)

        # Right panel: Snapshots tab + Firmware Library tab
        self._right_nb = ttk.Notebook(pw)

        snap_f = ttk.Frame(self._right_nb, padding=4)
        self._right_nb.add(snap_f, text="Snapshots")
        sbar = ttk.Frame(snap_f)
        sbar.pack(side=tk.BOTTOM, fill=tk.X, pady=4)
        self._busy_button(sbar, text="Set as Reference", command=self.set_reference).pack(side=tk.LEFT)
        self._snap_note = tk.StringVar()
        ttk.Label(sbar, textvariable=self._snap_note, foreground="#b00020").pack(side=tk.LEFT, padx=8)
        self._tree = ttk.Treeview(snap_f, columns=("ts", "label", "fw", "ref"),
                                    show="headings", selectmode="browse")
        self._tree.heading("ts",    text="Captured")
        self._tree.heading("label", text="Label")
        self._tree.heading("fw",    text="Firmware")
        self._tree.heading("ref",   text="")
        self._tree.column("ts",    width=175, stretch=False)
        self._tree.column("label", width=200)
        self._tree.column("fw",    width=70,  stretch=False)
        self._tree.column("ref",   width=55,  stretch=False)
        self._tree.tag_configure("unreadable", foreground="#b00020")
        snap_sb = ttk.Scrollbar(snap_f, orient=tk.VERTICAL, command=self._tree.yview)
        self._tree.configure(yscrollcommand=snap_sb.set)
        snap_sb.pack(side=tk.RIGHT, fill=tk.Y)
        self._tree.pack(side=tk.LEFT, fill=tk.BOTH, expand=True)
        self._tree.bind("<Double-1>", self._on_snapshot_double_click)

        fw_f = ttk.Frame(self._right_nb, padding=4)
        self._right_nb.add(fw_f, text="Firmware Library")
        self._build_fw_tab(fw_f)

        self._right_nb.bind("<<NotebookTabChanged>>", self._on_tab_change)
        pw.add(self._right_nb, weight=3)

    # ── Status, messages, errors ────────────────────────────────────────

    def _status(self, msg: str) -> None:
        self._status_var.set(msg.splitlines()[0] if msg else "")

    def _notice(self, text: str) -> None:
        """Append to the Messages box."""
        self._notes.configure(state=tk.NORMAL)
        self._notes.insert(tk.END, text.rstrip() + "\n")
        self._notes.see(tk.END)
        self._notes.configure(state=tk.DISABLED)

    def _error(self, title: str, text: str, parent=None) -> None:
        self._status(text if text.startswith("Error:") else f"Error: {text}")
        self._notice(f"{title}: {text}")
        messagebox.showerror(title, text, parent=parent or self.root)

    def _callback_failed(self, e: Exception) -> None:
        self._error("Internal error", f"{type(e).__name__}: {e}")

    def _tk_exception(self, exc, val, tb) -> None:
        traceback.print_exception(exc, val, tb, file=sys.stderr)
        self._error("Internal error", f"{exc.__name__}: {val}")

    # ── CLI bridge ──────────────────────────────────────────────────────

    def _set_busy(self, busy: bool) -> None:
        self.root.config(cursor="watch" if busy else "")
        alive = []
        for w in self._busy_widgets:
            try:
                w.configure(state=tk.DISABLED if busy else tk.NORMAL)
            except tk.TclError:
                continue   # its window was closed; forget it
            alive.append(w)
        self._busy_widgets = alive

    def _begin_archive_op(self, name: str) -> bool:
        """Claim the one slot for an operation that writes the archive (import,
        add, remove, download): two of them at once could fight over the same
        files. False (with the reason in the status line) if one is running."""
        if self._archive_op is not None:
            self._status(f"Wait for the {self._archive_op} to finish first.")
            return False
        self._archive_op = name
        return True

    def _end_archive_op(self) -> None:
        self._archive_op = None

    def _command(self, tail: list, with_data_dir: bool) -> list | None:
        try:
            binary = find_cli(_PROJECT)
        except CliNotFound as e:
            self._error("CLI not found", str(e))
            return None
        if not self._stale_checked:
            self._stale_checked = True
            warning = None if os.environ.get(ENV_CLI) else binary_stale_warning(binary, _PROJECT)
            if warning:
                self._notice(f"Warning: {warning}")
                self._status(f"Warning: {warning}")
        if not with_data_dir:
            return [str(binary), "--no-usb-reset", *tail]
        if self._data_dir is None:
            self._error("Data directory unknown",
                        "settings.json could not be read, so the data directory is not known. "
                        "Fix or remove the file (see the message above), then restart.")
            return None
        return [str(binary), *global_argv(self._data_dir), *tail]

    def _submit(self, fn, on_done) -> bool:
        """Run fn() in the background (one job at a time); on_done(result) runs
        here, after the buttons are enabled again. Warnings a successful CLI
        run wrote to stderr are put in the Messages box first."""
        def finish(result) -> None:
            self._set_busy(False)
            if isinstance(result, CliResult) and result.ok and result.stderr.strip():
                self._notice(result.stderr)
            on_done(result)

        if not self._cli.submit(fn, finish):
            self._status("Another operation is still running; wait for it to finish.")
            return False
        self._set_busy(True)
        self._cli_poll.start()
        return True

    def _call(self, tail: list, on_done, *, timeout: float = TIMEOUT_QUICK_S,
              stdin_data: str | None = None, with_data_dir: bool = True) -> bool:
        """Run the CLI with *tail* in the background; on_done(CliResult)."""
        cmd = self._command(tail, with_data_dir)
        if cmd is None:
            return False
        return self._submit(lambda: run_cli(cmd, stdin_data, timeout), on_done)

    # Public face for the vendor window.
    def call(self, tail: list, on_done, **kw) -> bool:
        return self._call(tail, on_done, **kw)

    # ── Settings ────────────────────────────────────────────────────────

    def _load_settings(self, then=None) -> None:
        def done(r: CliResult) -> None:
            if not r.ok:
                self._data_dir = None
                self._error("Settings", r.failure_text())
                return
            try:
                info = parse_settings_show(r.stdout)
            except CliOutputError as e:
                self._data_dir = None
                self._error("Settings", str(e))
                return
            self._data_dir = Path(info.effective_data_dir)
            if then:
                then()
        self._call(settings_show_argv(), done, with_data_dir=False)

    def _refresh_all(self) -> None:
        self._refresh_snapshots(then=self._refresh_firmware)

    # ── Discover ────────────────────────────────────────────────────────

    def discover(self) -> None:
        self._status("Discovering cameras…")

        def done(r: CliResult) -> None:
            if not r.ok:
                self._error("Discover failed", r.failure_text())
                return
            try:
                cams = parse_discover(r.stdout)
            except CliOutputError as e:
                self._error("Discover failed", str(e))
                return
            self._apply_cameras(cams)
            self._cameras_are_known_only = False
            self._status(f"Found {len(cams)} camera(s)." if cams else "No cameras found.")
            for c in cams:
                if c["baseline"] is False:
                    self._notice(f"{c['model']} {c['serial']}: no capture baseline ships for this model.")
        self._call(discover_argv(), done, timeout=TIMEOUT_CAMERA_S, with_data_dir=False)

    def _apply_cameras(self, cams: list) -> None:
        """Replace the camera list. The previously selected camera (by model and
        serial) stays selected if it is in the new list; otherwise the first
        camera is selected, and with no cameras nothing is, so no stale index
        can point at the wrong one."""
        keep = camera_key(self.cameras[self.selected]) if self.selected is not None else None
        self.cameras = cams
        self.selected = None
        self._cam_lb.delete(0, tk.END)
        for cam in cams:
            self._cam_lb.insert(tk.END, camera_label(cam))
        pick = next((i for i, c in enumerate(cams) if camera_key(c) == keep), 0 if cams else None)
        if pick is not None:
            self._cam_lb.selection_set(pick)
            self.selected = pick
        self._render_snapshots()

    def _selected_camera(self) -> dict | None:
        """The selected camera, or None (nothing selected, or a stale index)."""
        if self.selected is None or not 0 <= self.selected < len(self.cameras):
            return None
        return self.cameras[self.selected]

    # ── Vendor ops ──────────────────────────────────────────────────────
    #
    # Raw USB PTP vendor operations reverse-engineered from an NX Field WiFi
    # capture (see docs/nx-field-session-2026-07-09.md) — bypass the MAID SDK
    # entirely via `fleet vendor-read`/`fleet vendor-write-ftp`. Unlike
    # the rest of this GUI, these don't need a prior Discover: the CLI talks
    # straight to the USB device and only needs --serial when more than one
    # Nikon camera is attached.

    def _selected_serial(self) -> str | None:
        cam = self._selected_camera()
        return cam["serial"] if cam else None

    def vendor_ops(self) -> None:
        VendorOpsWindow(self.root, self)

    # ── Snapshots ───────────────────────────────────────────────────────

    def _on_cam_select(self, _evt) -> None:
        sel = self._cam_lb.curselection()
        if sel:
            if sel[0] != self.selected:
                # A label typed for one body must not end up on another body's snapshot.
                self._label.set("")
            self.selected = sel[0]
            self._render_snapshots()

    def _refresh_snapshots(self, then=None) -> None:
        """Re-read the snapshot list from the CLI (`ls --json`)."""
        def done(r: CliResult) -> None:
            if not r.ok:
                self._error("Listing snapshots failed", r.failure_text())
                return
            try:
                self._listing = parse_listing(r.stdout)
            except CliOutputError as e:
                self._error("Listing snapshots failed", str(e))
                return
            if self._cameras_are_known_only:
                self._apply_cameras(known_cameras(self._listing))
            else:
                self._render_snapshots()
            if not self._listing.dir_exists:
                self._status("No snapshots yet. Click Discover to find cameras.")
            elif not self.cameras:
                self._status("Click Discover to find cameras.")
            if then:
                then()
        self._call(ls_argv(), done)

    def _render_snapshots(self) -> None:
        self._tree.delete(*self._tree.get_children())
        self._snap_note.set("")
        if self._listing is None:
            return
        self._snap_note.set(listing_notes(self._listing))
        cam = self._selected_camera()
        if cam is not None:
            for r in rows_for_camera(self._listing, cam["model"], cam["serial"]):
                self._tree.insert("", tk.END, iid=r.file, values=(
                    r.captured_at[:19], r.label, r.firmware, "◀ ref" if r.is_reference else ""))
        for u in self._listing.unreadable:
            self._tree.insert("", tk.END, iid=u.file, tags=("unreadable",),
                              values=("(unreadable)", f"{u.file}: {u.error_kind}", "", ""))

    def take_snapshot(self) -> None:
        cam = self._selected_camera()
        if cam is None:
            messagebox.showwarning("No camera", "Select a camera first.", parent=self.root)
            return
        label = self._label.get().strip()
        self._status("Taking snapshot…")

        def done(r: CliResult) -> None:
            if not r.ok:
                self._error("Snapshot failed", r.failure_text())
                return
            self._status("Snapshot saved.")
            self._refresh_snapshots()
        self._call(snapshot_argv(cam["serial"], label), done, timeout=TIMEOUT_CAMERA_S)

    def _on_snapshot_double_click(self, evt) -> None:
        row = self._tree.identify_row(evt.y)
        if not row:
            return
        self._tree.selection_set(row)
        if "unreadable" in self._tree.item(row, "tags"):
            bad = next((u for u in self._listing.unreadable if u.file == row), None)
            self._error("Snapshot is unreadable",
                        f"{row}: {bad.error_kind}: {bad.error}" if bad else row)
            return
        if self._data_dir is None:
            return
        # A small local file, read here; the same validity rule as the CLI's `ls`.
        data, problem = read_snapshot_file(self._data_dir / "snapshots" / row)
        if problem:
            self._error("Could not open snapshot", f"{row}: {problem[0]}: {problem[1]}")
            return
        SnapshotDetailWindow(self.root, row, data)

    def set_reference(self) -> None:
        sel = self._tree.selection()
        if not sel:
            messagebox.showwarning("No snapshot", "Select a snapshot first.", parent=self.root)
            return
        fname = sel[0]   # iid == filename
        row = next((r for r in self._listing.rows if r.file == fname), None) if self._listing else None
        if row is None:
            messagebox.showwarning("Unreadable snapshot",
                                   "That file is not a readable snapshot, so it cannot be a reference.",
                                   parent=self.root)
            return
        current = listing_ref_marks(self._listing).get((row.model, row.serial), [])
        if fname in current:
            messagebox.showinfo("Reference", "That snapshot is already the reference.", parent=self.root)
            return
        if current and not messagebox.askyesno(
                "Replace the reference?",
                f"{row.model} {row.serial} already has a reference ({', '.join(current)}).\n"
                "Replace it with this snapshot? `check` will compare against the new one.",
                parent=self.root):
            return
        if self._data_dir is None:
            self._error("Set reference failed", "the data directory is not known (see the settings error above)")
            return
        try:
            tail = ref_set_argv(self._data_dir, fname)
        except ValueError as e:
            self._error("Set reference failed", str(e))
            return

        def done(r: CliResult) -> None:
            if not r.ok:
                self._error("Set reference failed", r.failure_text())
                return
            self._notice(r.stdout)    # says which reference it replaced, if any
            self._status("Reference set.")
            self._refresh_snapshots()
        self._call(tail, done)

    # ── Firmware library ────────────────────────────────────────────────

    def _build_fw_tab(self, parent: ttk.Frame) -> None:
        bf = ttk.Frame(parent)
        bf.pack(side=tk.BOTTOM, anchor=tk.W, pady=4)
        self._busy_button(bf, text="Add Firmware…", command=self.add_firmware).pack(side=tk.LEFT, padx=2)
        self._busy_button(bf, text="Remove", command=self.remove_firmware).pack(side=tk.LEFT, padx=2)
        self._dl_button = ttk.Button(bf, text="Download from Nikon…", command=self.download_firmware)
        self._dl_button.pack(side=tk.LEFT, padx=2)
        self._fw_note = tk.StringVar()
        ttk.Label(bf, textvariable=self._fw_note, foreground="#b00020").pack(side=tk.LEFT, padx=8)
        self._fw_tree = ttk.Treeview(parent, columns=("model", "version", "location", "status"),
                                      show="headings", selectmode="browse")
        self._fw_tree.heading("model",    text="Model")
        self._fw_tree.heading("version",  text="Version")
        self._fw_tree.heading("location", text="Archive")
        self._fw_tree.heading("status",   text="Status")
        self._fw_tree.column("model",    width=100, stretch=False)
        self._fw_tree.column("version",  width=70,  stretch=False)
        self._fw_tree.column("location", width=200)
        self._fw_tree.column("status",   width=160, stretch=False)
        self._fw_tree.tag_configure("problem", foreground="#b00020")
        fw_sb = ttk.Scrollbar(parent, orient=tk.VERTICAL, command=self._fw_tree.yview)
        self._fw_tree.configure(yscrollcommand=fw_sb.set)
        fw_sb.pack(side=tk.RIGHT, fill=tk.Y)
        self._fw_tree.pack(side=tk.LEFT, fill=tk.BOTH, expand=True)

    def _on_tab_change(self, _evt) -> None:
        if (self._right_nb.tab(self._right_nb.select(), "text") == "Firmware Library"
                and not self._cli.busy and self._data_dir is not None):
            self._refresh_firmware()

    def _refresh_firmware(self) -> None:
        """Re-read the firmware archive from the CLI (`firmware ls --json`)."""
        def done(r: CliResult) -> None:
            if not r.ok:
                self._error("Listing firmware failed", r.failure_text())
                return
            try:
                self._fw_listing = parse_firmware_listing(r.stdout)
            except CliOutputError as e:
                self._error("Listing firmware failed", str(e))
                return
            self._render_firmware()
        self._call(firmware_ls_argv(), done)

    def _render_firmware(self) -> None:
        self._fw_tree.delete(*self._fw_tree.get_children())
        fw = self._fw_listing
        if fw is None:
            return
        for iid, model, version, location, status in firmware_display_rows(fw):
            self._fw_tree.insert("", tk.END, iid=iid, values=(model, version, location, status),
                                 tags=() if status == "ok" else ("problem",))
        notes = []
        if fw.unreadable:
            notes.append(f"{len(fw.unreadable)} entries unreadable")
        if fw.skipped_dir_entries:
            notes.append(f"{fw.skipped_dir_entries} directory entries could not be read")
        self._fw_note.set("; ".join(notes))

    def _known_models(self) -> set:
        models = {c["model"] for c in self.cameras}
        if self._listing:
            models |= {r.model for r in self._listing.rows}
        if self._fw_listing:
            models |= {r.model for r in self._fw_listing.rows}
        return models

    def download_firmware(self) -> None:
        """Open Nikon's download page in `fleet firmware browse`'s own window.

        You accept the licence and click Download in that window; the CLI
        captures, checks and archives the file. It is a separate process
        because the web view needs its own main-thread event loop, and it
        runs on a worker thread of its own so the rest of this window stays
        usable meanwhile.
        """
        cam = self._selected_camera()
        model = simpledialog.askstring(
            "Download from Nikon",
            'Camera model to download the newest firmware for\n'
            '(as the camera list shows it, e.g. a Z 9 or Z6_3):',
            initialvalue=cam["model"] if cam else "", parent=self.root,
        )
        if not model or not model.strip():
            return
        model = model.strip()
        cmd = self._command(firmware_browse_argv(model), True)
        if cmd is None:
            return
        if not self._begin_archive_op("download"):
            return

        def done(r) -> None:
            self._end_archive_op()
            self._dl_button.configure(state=tk.NORMAL)
            if r.returncode is not None:
                ok, text = browse_outcome(r.returncode, r.stdout, r.stderr)
                if r.returncode == EXIT_ALREADY_ARCHIVED:
                    # `firmware browse` has no --force: say what the refusal means.
                    text += ("\n\nThat version is already in the library with different contents and "
                             "nothing was changed. Remove the existing entry first to replace it.")
            else:
                ok, text = False, r.failure_text()
            self._status(f"Nikon download for {model}: {'done' if ok else 'not archived'}.")
            if ok and r.stderr.strip():
                self._notice(r.stderr)
            (messagebox.showinfo if ok else messagebox.showerror)("Download from Nikon", text, parent=self.root)
            if not self._cli.busy:
                self._refresh_firmware()

        if not self._browse.submit(lambda: run_cli(cmd, None, TIMEOUT_BROWSE_S), done):
            self._end_archive_op()
            self._status("A download is already in progress.")
            return
        self._dl_button.configure(state=tk.DISABLED)
        self._status(f"A browser window is open for {model}: accept the licence there and click Download…")
        self._browse_poll.start()

    def add_firmware(self) -> None:
        # Delegates to `fleet firmware add` rather than copying the file
        # directly: the CLI owns the archive layout and its metadata.
        src = filedialog.askopenfilename(
            title="Add firmware to library",
            filetypes=[("Nikon firmware", "*.bin"), ("All files", "*.*")],
            parent=self.root,
        )
        if not src:
            return

        stem, guess_version = parse_fw_filename(Path(src).name)
        model = simpledialog.askstring(
            "Add Firmware",
            "Camera model, exactly as the camera reports it (the name in the camera list):",
            initialvalue=suggest_fw_model(stem, self._known_models()), parent=self.root,
        )
        if not model or not model.strip():
            return
        version = simpledialog.askstring(
            "Add Firmware", 'Firmware version (major.minor, e.g. "5.31"):',
            initialvalue=guess_version, parent=self.root,
        )
        if not version or not version.strip():
            return
        model, version = model.strip(), version.strip()
        if not self._begin_archive_op("firmware operation"):
            return

        def done(r: CliResult, forced: bool = False) -> None:
            if r.ok:
                self._end_archive_op()
                self._status(f"Archived {model} firmware {version}.")
                self._notice(r.stdout)
                self._refresh_firmware()
                return
            # Exit status 3 is the CLI's documented "already archived" refusal.
            if r.returncode == EXIT_ALREADY_ARCHIVED and not forced:
                if messagebox.askyesno(
                        "Already archived",
                        f"{model} {version} is already in the library. Overwrite?",
                        parent=self.root):
                    if self._call(firmware_add_argv(src, model, version, force=True),
                                  lambda r2: done(r2, True), timeout=TIMEOUT_FIRMWARE_ADD_S):
                        return
                self._end_archive_op()
                return
            self._end_archive_op()
            self._error("Add firmware failed", r.failure_text())

        if not self._call(firmware_add_argv(src, model, version), done, timeout=TIMEOUT_FIRMWARE_ADD_S):
            self._end_archive_op()

    def remove_firmware(self) -> None:
        sel = self._fw_tree.selection()
        if not sel or self._fw_listing is None:
            messagebox.showwarning("No selection", "Select a firmware entry first.", parent=self.root)
            return
        directory = sel[0]   # iid == the entry's directory as the CLI reported it
        label = self._fw_tree.set(directory, "location")
        if not messagebox.askyesno("Remove firmware",
                                    f"Remove {label} from the library?\nThis cannot be undone.",
                                    parent=self.root):
            return
        if self._data_dir is None:
            self._error("Remove failed", "the data directory is not known (see the settings error above)")
            return
        if not self._begin_archive_op("firmware operation"):
            return
        try:
            # Anchored to our own data directory, never to the `dir` the listing reported.
            remove_firmware_entry(self._data_dir / "firmware", directory)
        except (OSError, ValueError) as e:
            self._error("Remove failed", str(e))
            self._refresh_firmware()
            return
        finally:
            self._end_archive_op()
        self._status(f"Removed {label}.")
        self._refresh_firmware()

    # ── Preferences ─────────────────────────────────────────────────────

    def show_prefs(self) -> None:
        w = tk.Toplevel(self.root)
        w.title("Preferences")
        w.resizable(False, False)
        w.grab_set()

        ttk.Label(w, text="Data directory:", padding=8).grid(row=0, column=0, sticky=tk.W)
        dir_var = tk.StringVar(value=str(self._data_dir or ""))
        ttk.Entry(w, textvariable=dir_var, width=44).grid(row=0, column=1, padx=4, pady=8)
        ttk.Button(w, text="Browse…",
                   command=lambda: dir_var.set(
                       filedialog.askdirectory(initialdir=dir_var.get(), parent=w) or dir_var.get()
                   )).grid(row=0, column=2, padx=4)

        ttk.Separator(w, orient=tk.HORIZONTAL).grid(
            row=1, column=0, columnspan=3, sticky=tk.EW, padx=8, pady=4)

        ttk.Label(w, text="Data transfer:", padding=(8, 0)).grid(row=2, column=0, sticky=tk.W)
        bf = ttk.Frame(w)
        bf.grid(row=2, column=1, columnspan=2, sticky=tk.W, pady=4)
        for text, cmd in (("↑  Export all…", self.export_data), ("↓  Import archive…", self.import_data)):
            b = ttk.Button(bf, text=text, command=cmd)
            b.pack(side=tk.LEFT, padx=2)
            self._register_busy(b)

        ttk.Separator(w, orient=tk.HORIZONTAL).grid(
            row=3, column=0, columnspan=3, sticky=tk.EW, padx=8, pady=4)

        def _save() -> None:
            new_dir = dir_var.get().strip()

            def done(r: CliResult) -> None:
                if not r.ok:
                    self._error("Preferences", r.failure_text(), parent=w)
                    return
                self._status("Data dir saved.")
                w.destroy()
                self._load_settings(then=self._refresh_all)
            self._call(set_data_dir_argv(new_dir), done, with_data_dir=False)

        rf = ttk.Frame(w)
        rf.grid(row=4, column=0, columnspan=3, pady=8)
        save = ttk.Button(rf, text="Save", command=_save)
        save.pack(side=tk.LEFT, padx=4)
        self._register_busy(save)
        ttk.Button(rf, text="Cancel", command=w.destroy).pack(side=tk.LEFT)

    # ── Export / Import ─────────────────────────────────────────────────

    def _after_archive_job(self, result, what: str, then=None) -> None:
        """Common tail of an export/import job: a job that failed outright
        (an exception, not a report) is shown; a report is summarised."""
        if isinstance(result, CliResult):
            self._error(f"{what} failed", result.failure_text())
            return
        text = result.summary()
        self._status(text)
        self._notice(text)
        failed = bool(result.fatal)
        (messagebox.showerror if failed else messagebox.showinfo)(what, text, parent=self.root)
        if then:
            then()

    def export_data(self) -> None:
        if self._data_dir is None:
            self._error("Export", "the data directory is not known (see settings errors above)")
            return
        dest = filedialog.asksaveasfilename(
            defaultextension=".zip",
            filetypes=[("Zip archive", "*.zip")],
            initialfile="fleet-export.zip",
            parent=self.root,
        )
        if not dest:
            return
        dd = self._data_dir
        self._status("Exporting…")
        self._submit(lambda: export_archive(dd, Path(dest)),
                     lambda res: self._after_archive_job(res, "Export"))

    def import_data(self) -> None:
        if self._data_dir is None:
            self._error("Import", "the data directory is not known (see settings errors above)")
            return
        src = filedialog.askopenfilename(
            filetypes=[("Zip archive", "*.zip")],
            parent=self.root,
        )
        if not src:
            return
        dd = self._data_dir
        if not self._begin_archive_op("import"):
            return
        self._status("Importing…")

        def done(res) -> None:
            self._end_archive_op()
            self._after_archive_job(res, "Import", then=self._refresh_all)
        if not self._submit(lambda: import_archive(Path(src), dd), done):
            self._end_archive_op()


# ── Snapshot detail window ─────────────────────────────────────────────────

class SnapshotDetailWindow:
    def __init__(self, parent: tk.Tk, filename: str, data: dict) -> None:
        cam   = data.get("camera") if isinstance(data.get("camera"), dict) else {}
        label = data.get("label") or ""
        ts    = str(data.get("captured_at") or "")[:19]
        props = data.get("properties", {})

        title = f"{cam.get('model','')}  {cam.get('serial','')}  {ts}"
        if label:
            title += f"  [{label}]"

        w = tk.Toplevel(parent)
        w.title(title)
        w.geometry("740x520")

        # ── header ──────────────────────────────────────────────────────
        hdr = ttk.Frame(w, padding=(8, 6, 8, 4))
        hdr.pack(fill=tk.X)
        ttk.Label(hdr, text=title, font=("", 13, "bold")).pack(side=tk.LEFT)
        fw = cam.get("firmware", "")
        if fw:
            ttk.Label(hdr, text=f"fw {fw}", foreground="gray").pack(side=tk.LEFT, padx=8)
        ttk.Label(hdr, text=f"{len(props)} properties", foreground="gray").pack(side=tk.RIGHT)

        # ── search bar ──────────────────────────────────────────────────
        sf = ttk.Frame(w, padding=(8, 0, 8, 4))
        sf.pack(fill=tk.X)
        ttk.Label(sf, text="Filter:").pack(side=tk.LEFT)
        self._filter_var = tk.StringVar()
        filter_entry = ttk.Entry(sf, textvariable=self._filter_var, width=30)
        filter_entry.pack(side=tk.LEFT, padx=4)
        filter_entry.focus_set()
        ttk.Button(sf, text="✕", width=2,
                   command=lambda: self._filter_var.set("")).pack(side=tk.LEFT)

        # ── treeview ────────────────────────────────────────────────────
        cols = ("setting", "value", "code")
        tree = ttk.Treeview(w, columns=cols, show="headings", selectmode="browse")
        tree.heading("setting", text="Setting")
        tree.heading("value",   text="Value")
        tree.heading("code",    text="Code")
        tree.column("setting", width=260, stretch=False)
        tree.column("value",   width=380)
        tree.column("code",    width=70,  stretch=False, anchor=tk.E)
        vsb = ttk.Scrollbar(w, orient=tk.VERTICAL,   command=tree.yview)
        hsb = ttk.Scrollbar(w, orient=tk.HORIZONTAL, command=tree.xview)
        tree.configure(yscrollcommand=vsb.set, xscrollcommand=hsb.set)
        hsb.pack(side=tk.BOTTOM, fill=tk.X, padx=8)
        vsb.pack(side=tk.RIGHT,  fill=tk.Y)
        tree.pack(fill=tk.BOTH, expand=True, padx=(8, 0), pady=(0, 4))

        # ── status bar ──────────────────────────────────────────────────
        self._count_var = tk.StringVar()
        ttk.Label(w, textvariable=self._count_var, foreground="gray",
                  padding=(8, 2)).pack(side=tk.LEFT)

        # pre-process rows once
        self._all_rows: list[tuple[str, str, str]] = property_rows(props)

        self._tree = tree
        self._populate(self._all_rows)

        self._filter_var.trace_add("write", lambda *_: self._on_filter())

    def _populate(self, rows: list[tuple[str, str, str]]) -> None:
        self._tree.delete(*self._tree.get_children())
        for row in rows:
            self._tree.insert("", tk.END, values=row)
        self._count_var.set(f"{len(rows)} shown")

    def _on_filter(self) -> None:
        q = self._filter_var.get().lower()
        if not q:
            self._populate(self._all_rows)
            return
        filtered = [r for r in self._all_rows
                    if q in r[0].lower() or q in r[1].lower()]
        self._populate(filtered)


# ── Vendor ops window ───────────────────────────────────────────────────────

def _make_output_box(parent: tk.Widget, height: int = 3) -> tk.Text:
    """A small read-only, word-wrapped box for command output/errors —
    plain output can run to a few hundred characters (e.g. a dry-run's
    encoded hex blob), too long for a Label to lay out well."""
    box = tk.Text(parent, height=height, width=56, wrap=tk.WORD, state=tk.DISABLED,
                  background="#f5f5f5", relief=tk.SUNKEN, borderwidth=1, font=("Menlo", 10))
    return box


def _set_output(box: tk.Text, text: str) -> None:
    box.configure(state=tk.NORMAL)
    box.delete("1.0", tk.END)
    box.insert("1.0", text)
    box.configure(state=tk.DISABLED)


class VendorOpsWindow:
    """Raw USB PTP vendor operations — reverse-engineered from an NX Field
    WiFi capture, bypassing the MAID SDK entirely. See
    docs/nx-field-session-2026-07-09.md for what these are and how they
    were found.
    """

    def __init__(self, parent: tk.Tk, app: "FleetApp") -> None:
        self.app = app
        w = tk.Toplevel(parent)
        w.title("Vendor Ops (raw USB PTP)")
        w.resizable(False, False)
        w.grab_set()
        self._window = w

        cam = app._selected_camera()
        target = (f"Target: {cam['model']} {cam['serial']}" if cam
                  else "Target: (auto-detected — only one Nikon camera on USB)")
        ttk.Label(w, text=target, foreground="gray", padding=(8, 8, 8, 0)).pack(anchor=tk.W)
        # Fixed for this window's lifetime: the camera list can change under it.
        self._serial = cam["serial"] if cam else None
        self._camera = dict(cam) if cam else None

        # ── Read ────────────────────────────────────────────────────────
        read_f = ttk.LabelFrame(w, text="Read Vendor Property (0x943B)", padding=8)
        read_f.pack(fill=tk.X, padx=8, pady=6)
        ttk.Label(read_f, text="Property code:").grid(row=0, column=0, sticky=tk.W)
        self._code_var = tk.StringVar(value="0xD053")
        ttk.Entry(read_f, textvariable=self._code_var, width=12).grid(row=0, column=1, padx=4, sticky=tk.W)
        read_btn = ttk.Button(read_f, text="Read", command=self._do_read)
        read_btn.grid(row=0, column=2, padx=4)
        self._read_box = _make_output_box(read_f, height=2)
        self._read_box.grid(row=1, column=0, columnspan=3, sticky=tk.EW, pady=(6, 0))

        # ── Write FTP profile ───────────────────────────────────────────
        write_f = ttk.LabelFrame(w, text="Write FTP Profile (0x90EE)", padding=8)
        write_f.pack(fill=tk.X, padx=8, pady=(0, 8))

        self._fields: dict[str, tk.StringVar] = {}
        rows = [(key, label, "", None) for key, _flag, label in FTP_TEXT_FIELDS]
        rows.insert(4, ("port", "Port:", "21", None))
        rows.append((FTP_PASSWORD_KEY, "Password:", "", "*"))
        for row, (key, label, default, show) in enumerate(rows):
            ttk.Label(write_f, text=label).grid(row=row, column=0, sticky=tk.W, pady=2)
            var = tk.StringVar(value=default)
            self._fields[key] = var
            entry_kwargs = {"show": show} if show else {}
            ttk.Entry(write_f, textvariable=var, width=30, **entry_kwargs).grid(
                row=row, column=1, sticky=tk.W, padx=4, pady=2)

        btn_row = len(rows)
        bf = ttk.Frame(write_f)
        bf.grid(row=btn_row, column=0, columnspan=2, pady=(8, 0), sticky=tk.W)
        preview = ttk.Button(bf, text="Preview (dry run)", command=lambda: self._do_write(dry_run=True))
        preview.pack(side=tk.LEFT, padx=(0, 4))
        write = ttk.Button(bf, text="Write to Camera…", command=lambda: self._do_write(dry_run=False))
        write.pack(side=tk.LEFT)
        refusal = self._write_refusal()
        if refusal:
            write.configure(state=tk.DISABLED)      # not a busy widget: it stays disabled
            ttk.Label(bf, text=refusal, foreground="#b00020", wraplength=330).pack(side=tk.LEFT, padx=8)

        self._write_box = _make_output_box(write_f, height=4)
        self._write_box.grid(row=btn_row + 1, column=0, columnspan=2, sticky=tk.EW, pady=(6, 0))

        for b in (read_btn, preview) + (() if refusal else (write,)):
            app._register_busy(b)

    def _write_refusal(self) -> str | None:
        """Why a real write is not allowed now, or None. A camera that is in the
        list only because of old snapshots has not been seen on USB in this
        session, so a write aimed at its stored serial is not safe to offer."""
        if self._camera is not None and self.app._cameras_are_known_only:
            return ("Writing is disabled: this camera is listed only from saved snapshots. "
                    "Close this window and click Discover so the body is seen on USB.")
        return None

    def _target_text(self) -> str:
        cam = self._camera
        if cam is None:
            return "the only Nikon camera on USB (auto-detected)"
        text = f"{cam['model']}  {cam['serial']}"
        if is_id_fallback(cam["serial"]):
            text += "\n  (an id-N serial: the one attached body with no readable serial)"
        return text

    def _do_read(self) -> None:
        try:
            code = parse_propcode(self._code_var.get())
        except ValueError as e:
            _set_output(self._read_box, f"Invalid property code: {e}")
            return

        def done(r: CliResult) -> None:
            _set_output(self._read_box, join_output(r.stdout, r.stderr) if r.ok
                        else r.failure_text())
        self.app.call(vendor_read_argv(code, self._serial), done,
                      timeout=TIMEOUT_CAMERA_S, with_data_dir=False)

    def _form_values(self) -> dict | None:
        """The form's values, or None with the reason shown. Only the port is
        checked here: which text fields are required, and how long each may
        be, is the CLI's rule, and its refusal (which names the field) is
        shown as it comes."""
        try:
            return ftp_form_values({k: v.get() for k, v in self._fields.items()})
        except ValueError as e:
            _set_output(self._write_box, f"Invalid port: {e}")
            return None

    def _do_write(self, dry_run: bool) -> None:
        refusal = None if dry_run else self._write_refusal()
        if refusal:
            _set_output(self._write_box, f"Not sent. {refusal}")
            return
        values = self._form_values()
        if values is None:
            return
        password = values[FTP_PASSWORD_KEY]
        # The password goes on stdin (--password-stdin), not in the process list.
        preview_argv = vendor_write_ftp_argv(values, self._serial, dry_run=True)

        def after_preview(r: CliResult) -> None:
            if not r.ok:
                _set_output(self._write_box, f"Not sent. {r.failure_text()}")
                return
            if dry_run:
                _set_output(self._write_box, join_output(r.stdout, r.stderr))
                return
            if not messagebox.askyesno(
                "Write FTP Profile",
                "This overwrites the FTP profile stored on the camera:\n\n"
                f"  Camera: {self._target_text()}\n"
                f"  Name: {values['profile_name']}\n"
                f"  Host: {values['host']}:{values['port']}\n"
                f"  User: {values['username']}\n\n"
                "Continue?",
                parent=self._window,
            ):
                _set_output(self._write_box, "Cancelled; nothing was sent.")
                return
            self.app.call(vendor_write_ftp_argv(values, self._serial, dry_run=False), after_write,
                          stdin_data=password, timeout=TIMEOUT_CAMERA_S, with_data_dir=False)

        def after_write(r: CliResult) -> None:
            if r.ok:
                _set_output(self._write_box, join_output(r.stdout, r.stderr))
            else:
                _set_output(self._write_box,
                            ftp_write_failure_text(r.returncode, r.timed_out, r.failure_text()))

        # Every write is validated first by the CLI's own --dry-run (it
        # touches no camera), so a refusal never reaches the confirm dialog.
        self.app.call(preview_argv, after_preview, stdin_data=password, with_data_dir=False)


# ── Entry point ────────────────────────────────────────────────────────────

if __name__ == "__main__":
    root = tk.Tk()
    FleetApp(root)
    root.mainloop()
