#!/usr/bin/env python3
"""Fleet GUI — Nikon camera fleet manager (tkinter / subprocess frontend)."""

import json
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path
import tkinter as tk
from tkinter import ttk, messagebox, filedialog, simpledialog

from fleet_lib import strip_sdk_prefix, accept_zip_entry, parse_fw_filename, fmt_cap_value, model_slug, parse_propcode, is_valid_u16

# ── Paths ──────────────────────────────────────────────────────────────────

_PROJECT = Path(__file__).resolve().parent.parent   # repo root

# Must match src/firmware.rs's FIRMWARE_META_FORMAT_VERSION.
_FIRMWARE_META_FORMAT_VERSION = 1

def _fleet_bin() -> Path:
    for p in [_PROJECT / "target" / "release" / "nikon-fleet",
              _PROJECT / "target" / "debug"   / "nikon-fleet"]:
        if p.exists():
            return p
    raise RuntimeError(
        "fleet binary not found.\n"
        "Run `cargo build --release` in the project root first."
    )

# Settings file location must match the Rust GUI's settings_path()
# (gui/src/main.rs), which uses dirs::config_dir() → ~/Library/Application
# Support on macOS. The two GUIs share one settings.json so a data-dir
# change made in either is visible to both.
_SETTINGS_PATH = Path.home() / "Library" / "Application Support" / "net.blw.fleet" / "settings.json"

# (mtime, data_dir) — data_dir() is called on nearly every user action
# (discover, load-snapshots, set-reference, export, import, show-prefs), but
# the file only actually changes via _save_data_dir's Save button, or the
# Rust GUI writing the same shared settings.json. Cache on mtime rather than
# unconditionally, since that concurrent-writer case is real, not
# hypothetical (both GUIs share this exact file — see settings_path() in
# gui/src/main.rs).
_data_dir_cache: tuple[float, Path] | None = None

def _data_dir() -> Path:
    global _data_dir_cache
    try:
        mtime = _SETTINGS_PATH.stat().st_mtime
    except OSError:
        return Path.home() / "Library" / "Application Support" / "net.blw.fleet"
    if _data_dir_cache is not None and _data_dir_cache[0] == mtime:
        return _data_dir_cache[1]
    try:
        s = json.loads(_SETTINGS_PATH.read_text())
        result = Path(s["data_dir"]) if s.get("data_dir") else Path.home() / "Library" / "Application Support" / "net.blw.fleet"
    except Exception:
        result = Path.home() / "Library" / "Application Support" / "net.blw.fleet"
    _data_dir_cache = (mtime, result)
    return result

def _firmware_dir() -> Path:
    return _data_dir() / "firmware"

def _save_data_dir(new_dir: str) -> None:
    global _data_dir_cache
    _data_dir_cache = None
    _SETTINGS_PATH.parent.mkdir(parents=True, exist_ok=True)
    try:
        s = json.loads(_SETTINGS_PATH.read_text())
    except Exception:
        s = {}
    s["data_dir"] = new_dir or None
    _SETTINGS_PATH.write_text(json.dumps(s, indent=2))

# ── App ────────────────────────────────────────────────────────────────────

class FleetApp:
    def __init__(self, root: tk.Tk) -> None:
        self.root = root
        self.root.title("Fleet — Nikon Camera Manager")
        self.root.geometry("860x520")
        self.cameras: list[dict] = []
        self.selected: int | None = None
        self._build_ui()
        self._scan_known_cameras()
        self._load_firmware()
        if not self.cameras:
            self._status("Click Discover to find cameras.")
        self.root.after(100, self.root.focus_force)

    # ── UI ──────────────────────────────────────────────────────────────

    def _build_ui(self) -> None:
        bar = ttk.Frame(self.root, padding=4)
        bar.pack(fill=tk.X)
        ttk.Button(bar, text="⟳  Discover",      command=self.discover).pack(side=tk.LEFT)
        ttk.Separator(bar, orient=tk.VERTICAL).pack(side=tk.LEFT, fill=tk.Y, padx=4)
        ttk.Label(bar, text="Label:").pack(side=tk.LEFT)
        self._label = tk.StringVar()
        self._label_entry = tk.Entry(bar, textvariable=self._label, width=18)
        self._label_entry.pack(side=tk.LEFT, padx=2)
        ttk.Button(bar, text="Take Snapshot",    command=self.take_snapshot).pack(side=tk.LEFT, padx=2)
        ttk.Separator(bar, orient=tk.VERTICAL).pack(side=tk.LEFT, fill=tk.Y, padx=4)
        ttk.Button(bar, text="🔧  Vendor Ops",   command=self.vendor_ops).pack(side=tk.LEFT)
        ttk.Separator(bar, orient=tk.VERTICAL).pack(side=tk.LEFT, fill=tk.Y, padx=4)
        ttk.Button(bar, text="⚙  Preferences",  command=self.show_prefs).pack(side=tk.LEFT)
        self._status_var = tk.StringVar()
        ttk.Label(bar, textvariable=self._status_var, foreground="gray").pack(side=tk.LEFT, padx=8)

        pw = ttk.PanedWindow(self.root, orient=tk.HORIZONTAL)
        pw.pack(fill=tk.BOTH, expand=True, padx=4, pady=4)

        # Camera list
        cam_f = ttk.LabelFrame(pw, text="Cameras", padding=4)
        self._cam_lb = tk.Listbox(cam_f, width=26, activestyle="none",
                                   selectbackground="#4a9eff", selectforeground="white")
        self._cam_lb.pack(fill=tk.BOTH, expand=True)
        self._cam_lb.bind("<<ListboxSelect>>", self._on_cam_select)
        pw.add(cam_f, weight=1)

        # Right panel: Snapshots tab + Firmware Library tab
        self._right_nb = ttk.Notebook(pw)

        snap_f = ttk.Frame(self._right_nb, padding=4)
        self._right_nb.add(snap_f, text="Snapshots")
        ttk.Button(snap_f, text="Set as Reference",
                   command=self.set_reference).pack(side=tk.BOTTOM, anchor=tk.W, pady=4)
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

    def _scan_known_cameras(self) -> None:
        """Populate camera list from existing snapshot files, without needing a live camera."""
        snap_dir = _data_dir() / "snapshots"
        if not snap_dir.exists():
            return
        seen: dict[str, dict] = {}
        for f in snap_dir.glob("*.json"):
            try:
                cam = json.loads(f.read_text())["camera"]
                if cam["serial"] not in seen:
                    seen[cam["serial"]] = {"model": cam["model"], "serial": cam["serial"],
                                           "firmware": cam.get("firmware", "")}
            except Exception as e:
                print(f"warning: could not read snapshot {f}: {e}", file=sys.stderr)
        if not seen:
            return
        self.cameras = list(seen.values())
        self._cam_lb.delete(0, tk.END)
        for cam in self.cameras:
            self._cam_lb.insert(tk.END, f"  {cam['model']}  ·  {cam['serial']}")
        self._status(f"Loaded {len(self.cameras)} known camera(s). Click Discover to connect.")
        self._cam_lb.selection_set(0)
        self._on_cam_select(None)

    def _status(self, msg: str) -> None:
        self._status_var.set(msg)
        self.root.update_idletasks()

    # ── CLI bridge ──────────────────────────────────────────────────────

    def _run(self, *args: str, stdin_data: str | None = None) -> str:
        cmd = [str(_fleet_bin()), "--no-usb-reset", "--data-dir", str(_data_dir())] + list(args)
        r = subprocess.run(cmd, capture_output=True, text=True, input=stdin_data)
        if r.returncode != 0:
            raise RuntimeError(r.stderr.strip() or f"fleet exited {r.returncode}")
        return r.stdout

    # ── Discover ────────────────────────────────────────────────────────

    def discover(self) -> None:
        self._status("Discovering cameras…")
        self.root.config(cursor="watch")
        self.root.update()
        try:
            out = self._run("discover", "--json")
            self.cameras = json.loads(strip_sdk_prefix(out))["cameras"]
            self._cam_lb.delete(0, tk.END)
            for cam in self.cameras:
                self._cam_lb.insert(tk.END, f"  {cam['model']}  ·  {cam['serial']}")
            self._status(f"Found {len(self.cameras)} camera(s).")
            if self.cameras:
                self._cam_lb.selection_set(0)
                self._on_cam_select(None)
        except Exception as e:
            self._status(f"Error: {e}")
            messagebox.showerror("Discover failed", str(e), parent=self.root)
        finally:
            self.root.config(cursor="")

    # ── Vendor ops ──────────────────────────────────────────────────────
    #
    # Raw USB PTP vendor operations reverse-engineered from an NX Field WiFi
    # capture (see docs/nx-field-session-2026-07-09.md) — bypass the MAID
    # SDK entirely via `fleet vendor-read`/`fleet vendor-write-ftp`. Unlike
    # the rest of this GUI, these don't need a prior Discover: the CLI talks
    # straight to the USB device and only needs --serial when more than one
    # Nikon camera is attached.

    def _selected_serial(self) -> str | None:
        if self.selected is None:
            return None
        return self.cameras[self.selected]["serial"]

    def vendor_ops(self) -> None:
        VendorOpsWindow(self.root, self)

    # ── Snapshots ───────────────────────────────────────────────────────

    def _on_cam_select(self, _evt) -> None:
        sel = self._cam_lb.curselection()
        if sel:
            self.selected = sel[0]
            self._load_snapshots()

    def _load_snapshots(self) -> None:
        self._tree.delete(*self._tree.get_children())
        if self.selected is None:
            return
        serial   = self.cameras[self.selected]["serial"]
        model    = self.cameras[self.selected]["model"]
        dd       = _data_dir()
        snap_dir = dd / "snapshots"
        ref_dir  = dd / "references"
        if not snap_dir.exists():
            return
        # Read the reference file for this camera (CLI naming: {model_slug}_{serial}.json).
        ref_captured_at: str | None = None
        if ref_dir.exists():
            ref_name = f"{model_slug(model)}_{serial}.json"
            ref_path = ref_dir / ref_name
            if ref_path.exists():
                try:
                    ref_captured_at = json.loads(ref_path.read_text()).get("captured_at", "")[:19]
                except Exception:
                    pass
        rows = []
        for f in snap_dir.glob("*.json"):
            try:
                d = json.loads(f.read_text())
                if d["camera"]["serial"] == serial:
                    rows.append((d.get("captured_at", "")[:19],
                                 d.get("label") or "",
                                 d["camera"].get("firmware", ""),
                                 f.name))
            except Exception as e:
                print(f"warning: could not read snapshot {f}: {e}", file=sys.stderr)
        rows.sort(reverse=True)
        for ts, label, fw, fname in rows:
            ref_mark = "◀ ref" if ref_captured_at and ts == ref_captured_at else ""
            self._tree.insert("", tk.END, iid=fname, values=(ts, label, fw, ref_mark))

    def take_snapshot(self) -> None:
        if self.selected is None:
            messagebox.showwarning("No camera", "Select a camera first.", parent=self.root)
            return
        serial = self.cameras[self.selected]["serial"]
        label  = self._label.get().strip()
        self._status("Taking snapshot…")
        self.root.config(cursor="watch")
        self.root.update()
        try:
            args = ["snapshot", "--serial", serial]
            if label:
                args += ["--label", label]
            self._run(*args)
            self._load_snapshots()
            self._status("Snapshot saved.")
        except Exception as e:
            self._status(f"Error: {e}")
            messagebox.showerror("Snapshot failed", str(e), parent=self.root)
        finally:
            self.root.config(cursor="")

    def _on_snapshot_double_click(self, evt) -> None:
        row = self._tree.identify_row(evt.y)
        if not row:
            return
        self._tree.selection_set(row)
        fname = row   # iid == filename
        path  = _data_dir() / "snapshots" / fname
        try:
            data = json.loads(path.read_text())
        except Exception as e:
            messagebox.showerror("Could not open snapshot", str(e), parent=self.root)
            return
        SnapshotDetailWindow(self.root, fname, data)

    def set_reference(self) -> None:
        sel = self._tree.selection()
        if not sel:
            messagebox.showwarning("No snapshot", "Select a snapshot first.", parent=self.root)
            return
        fname   = sel[0]   # iid == filename
        dd      = _data_dir()
        ref_dir = dd / "references"
        ref_dir.mkdir(parents=True, exist_ok=True)
        try:
            snap = json.loads((dd / "snapshots" / fname).read_text())
            cam  = snap["camera"]
            # Match `fleet ref set` naming so `fleet check` finds the reference.
            ref_fname = f"{model_slug(cam['model'])}_{cam['serial']}.json"
            shutil.copy2(dd / "snapshots" / fname, ref_dir / ref_fname)
        except Exception as e:
            # Broadened from `except OSError`: this block now also does
            # json.loads() and dict indexing (added when the canonical
            # filename computation moved in-line), so a malformed or
            # partially-written snapshot file raises JSONDecodeError or
            # KeyError — neither is an OSError subclass, so they used to
            # propagate uncaught out of this callback instead of showing
            # the error dialog this except clause exists to provide.
            messagebox.showerror("Set reference failed", str(e), parent=self.root)
            return
        self._load_snapshots()
        self._status("Reference set.")

    # ── Firmware library ────────────────────────────────────────────────

    def _build_fw_tab(self, parent: ttk.Frame) -> None:
        bf = ttk.Frame(parent)
        bf.pack(side=tk.BOTTOM, anchor=tk.W, pady=4)
        ttk.Button(bf, text="Add Firmware…", command=self.add_firmware).pack(side=tk.LEFT, padx=2)
        ttk.Button(bf, text="Remove",        command=self.remove_firmware).pack(side=tk.LEFT, padx=2)
        self._fw_tree = ttk.Treeview(parent, columns=("model", "version", "filename"),
                                      show="headings", selectmode="browse")
        self._fw_tree.heading("model",    text="Model")
        self._fw_tree.heading("version",  text="Version")
        self._fw_tree.heading("filename", text="Filename")
        self._fw_tree.column("model",    width=100, stretch=False)
        self._fw_tree.column("version",  width=70,  stretch=False)
        self._fw_tree.column("filename", width=260)
        fw_sb = ttk.Scrollbar(parent, orient=tk.VERTICAL, command=self._fw_tree.yview)
        self._fw_tree.configure(yscrollcommand=fw_sb.set)
        fw_sb.pack(side=tk.RIGHT, fill=tk.Y)
        self._fw_tree.pack(side=tk.LEFT, fill=tk.BOTH, expand=True)

    def _on_tab_change(self, _evt) -> None:
        if self._right_nb.tab(self._right_nb.select(), "text") == "Firmware Library":
            self._load_firmware()

    def _load_firmware(self) -> None:
        self._fw_tree.delete(*self._fw_tree.get_children())
        self._fw_meta: dict[str, dict] = {}  # iid → full metadata dict
        fw_dir = _firmware_dir()
        if not fw_dir.exists():
            return
        # Nested archive layout: firmware/{model_slug}/{version}/metadata.json
        for meta_file in sorted(fw_dir.rglob("metadata.json")):
            try:
                meta = json.loads(meta_file.read_text())
                # Must match src/firmware.rs's list_archives(), which skips
                # (rather than trusts) a metadata.json whose format_version
                # doesn't match — otherwise a future format bump is shown
                # here as valid while the real CLI correctly hides it.
                found = meta.get("format_version")
                if found != _FIRMWARE_META_FORMAT_VERSION:
                    print(
                        f"warning: skipping {meta_file} (format_version={found}, "
                        f"expected {_FIRMWARE_META_FORMAT_VERSION})",
                        file=sys.stderr,
                    )
                    continue
                model   = meta.get("model", "")
                version = meta.get("firmware_version", "")
                slug    = meta_file.parent.parent.name
                ver     = meta_file.parent.name
                iid     = f"nested|{slug}|{ver}"
                self._fw_meta[iid] = meta
                self._fw_tree.insert("", tk.END, iid=iid,
                                     values=(model, version, f"{slug}/{ver}/firmware.bin"))
            except Exception as e:
                print(f"warning: could not read firmware metadata {meta_file}: {e}", file=sys.stderr)
                continue
        # Legacy flat layout: firmware/*.bin (added via old GUI Add button)
        for f in sorted(fw_dir.glob("*.bin")):
            model, version = parse_fw_filename(f.name)
            self._fw_tree.insert("", tk.END, iid=f"flat|{f.name}",
                                 values=(model, version, f.name))

    def add_firmware(self) -> None:
        # Delegates to `fleet firmware add` rather than copying the file
        # directly — a prior version wrote straight into the flat legacy
        # layout (firmware/<basename>.bin, no metadata.json), which
        # `fleet firmware ls/pin/rollback` and export/import all silently
        # ignore, since they only recognize the nested
        # firmware/{model_slug}/{version}/metadata.json archive format.
        src = filedialog.askopenfilename(
            title="Add firmware to library",
            filetypes=[("Nikon firmware", "*.bin"), ("All files", "*.*")],
            parent=self.root,
        )
        if not src:
            return

        guess_model, guess_version = parse_fw_filename(Path(src).name)
        model = simpledialog.askstring(
            "Add Firmware", 'Camera model (e.g. "Z 9"):',
            initialvalue=guess_model.replace("_", " "), parent=self.root,
        )
        if not model:
            return
        version = simpledialog.askstring(
            "Add Firmware", 'Firmware version (e.g. "5.31"):',
            initialvalue=guess_version, parent=self.root,
        )
        if not version:
            return

        args = ["firmware", "add", src, "--model", model, "--version", version]
        try:
            self._run(*args)
        except RuntimeError as e:
            if "already archived" not in str(e).lower() and "already exists" not in str(e).lower():
                self._status(f"Error: {e}")
                messagebox.showerror("Add firmware failed", str(e), parent=self.root)
                return
            if not messagebox.askyesno(
                "Already archived",
                f"{model} {version} is already in the library. Overwrite?",
                parent=self.root,
            ):
                return
            try:
                self._run(*args, "--force")
            except RuntimeError as e2:
                self._status(f"Error: {e2}")
                messagebox.showerror("Add firmware failed", str(e2), parent=self.root)
                return

        self._load_firmware()
        self._status(f"Archived {model} firmware {version}.")

    def remove_firmware(self) -> None:
        sel = self._fw_tree.selection()
        if not sel:
            messagebox.showwarning("No selection", "Select a firmware file first.", parent=self.root)
            return
        iid = sel[0]
        fw_dir = _firmware_dir()
        if iid.startswith("nested|"):
            _, slug, ver = iid.split("|", 2)
            target = fw_dir / slug / ver
            label  = f"{slug}/{ver}"
        else:
            _, fname = iid.split("|", 1)
            target = fw_dir / fname
            label  = fname
        if not messagebox.askyesno("Remove firmware",
                                    f"Remove {label} from the library?\nThis cannot be undone.",
                                    parent=self.root):
            return
        try:
            if target.is_dir():
                shutil.rmtree(target)
            else:
                target.unlink()
        except OSError as e:
            messagebox.showerror("Remove failed", str(e), parent=self.root)
            return
        self._load_firmware()
        self._status(f"Removed {label}.")

    # ── Preferences ─────────────────────────────────────────────────────

    def show_prefs(self) -> None:
        w = tk.Toplevel(self.root)
        w.title("Preferences")
        w.resizable(False, False)
        w.grab_set()

        ttk.Label(w, text="Data directory:", padding=8).grid(row=0, column=0, sticky=tk.W)
        dir_var = tk.StringVar(value=str(_data_dir()))
        ttk.Entry(w, textvariable=dir_var, width=44).grid(row=0, column=1, padx=4, pady=8)
        ttk.Button(w, text="Browse…",
                   command=lambda: dir_var.set(
                       filedialog.askdirectory(initialdir=dir_var.get()) or dir_var.get()
                   )).grid(row=0, column=2, padx=4)

        ttk.Separator(w, orient=tk.HORIZONTAL).grid(
            row=1, column=0, columnspan=3, sticky=tk.EW, padx=8, pady=4)

        ttk.Label(w, text="Data transfer:", padding=(8, 0)).grid(row=2, column=0, sticky=tk.W)
        bf = ttk.Frame(w)
        bf.grid(row=2, column=1, columnspan=2, sticky=tk.W, pady=4)
        ttk.Button(bf, text="↑  Export all…",       command=self.export_data).pack(side=tk.LEFT, padx=2)
        ttk.Button(bf, text="↓  Import archive…",   command=self.import_data).pack(side=tk.LEFT, padx=2)

        ttk.Separator(w, orient=tk.HORIZONTAL).grid(
            row=3, column=0, columnspan=3, sticky=tk.EW, padx=8, pady=4)

        def _save():
            _save_data_dir(dir_var.get().strip())
            self._load_snapshots()
            self._load_firmware()
            self._status(f"Data dir saved.")
            w.destroy()

        rf = ttk.Frame(w)
        rf.grid(row=4, column=0, columnspan=3, pady=8)
        ttk.Button(rf, text="Save",   command=_save).pack(side=tk.LEFT, padx=4)
        ttk.Button(rf, text="Cancel", command=w.destroy).pack(side=tk.LEFT)

    # ── Export / Import ─────────────────────────────────────────────────

    def export_data(self) -> None:
        dest = filedialog.asksaveasfilename(
            defaultextension=".zip",
            filetypes=[("Zip archive", "*.zip")],
            initialfile="fleet-export.zip",
            parent=self.root,
        )
        if not dest:
            return
        dd    = _data_dir()
        count = 0
        with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as zf:
            for folder in ("snapshots", "references"):
                d = dd / folder
                if d.exists():
                    for f in d.glob("*.json"):
                        zf.write(f, f"{folder}/{f.name}")
                        count += 1
            fw_dir = dd / "firmware"
            if fw_dir.exists():
                for f in fw_dir.rglob("firmware.bin"):
                    zf.write(f, str(f.relative_to(dd)), compress_type=zipfile.ZIP_STORED)
                    count += 1
                for f in fw_dir.rglob("metadata.json"):
                    zf.write(f, str(f.relative_to(dd)))
                    count += 1
        self._status(f"Exported {count} file(s) → {Path(dest).name}")

    def import_data(self) -> None:
        src = filedialog.askopenfilename(
            filetypes=[("Zip archive", "*.zip")],
            parent=self.root,
        )
        if not src:
            return
        dd = _data_dir()
        snaps = refs = fw = 0
        with zipfile.ZipFile(src) as zf:
            for name in zf.namelist():
                if not accept_zip_entry(name):
                    continue
                out = dd / Path(name)
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_bytes(zf.read(name))
                parts = Path(name).parts
                if parts[0] == "snapshots":   snaps += 1
                elif parts[0] == "references": refs  += 1
                else:                          fw    += 1
        self._load_snapshots()
        self._load_firmware()
        self._status(f"Imported {snaps} snapshot(s), {refs} reference(s), {fw} firmware file(s).")

# ── Snapshot detail window ─────────────────────────────────────────────────

_CAP_PREFIX = "kNkMAIDCapability_"

class SnapshotDetailWindow:
    def __init__(self, parent: tk.Tk, filename: str, data: dict) -> None:
        cam   = data.get("camera", {})
        label = data.get("label") or ""
        ts    = (data.get("captured_at") or "")[:19]
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
        self._all_rows: list[tuple[str, str, str]] = []
        for name, entry in sorted(props.items()):
            display = name.removeprefix(_CAP_PREFIX)
            value   = fmt_cap_value(entry.get("value"))
            code    = f"{entry.get('code', 0):#06x}"
            self._all_rows.append((display, value, code))

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

        serial = app._selected_serial()
        target = f"Target: {serial}" if serial else "Target: (auto-detected — only one Nikon camera on USB)"
        ttk.Label(w, text=target, foreground="gray", padding=(8, 8, 8, 0)).pack(anchor=tk.W)

        # ── Read ────────────────────────────────────────────────────────
        read_f = ttk.LabelFrame(w, text="Read Vendor Property (0x943B)", padding=8)
        read_f.pack(fill=tk.X, padx=8, pady=6)
        ttk.Label(read_f, text="Property code:").grid(row=0, column=0, sticky=tk.W)
        self._code_var = tk.StringVar(value="0xD053")
        ttk.Entry(read_f, textvariable=self._code_var, width=12).grid(row=0, column=1, padx=4, sticky=tk.W)
        ttk.Button(read_f, text="Read", command=self._do_read).grid(row=0, column=2, padx=4)
        self._read_box = _make_output_box(read_f, height=2)
        self._read_box.grid(row=1, column=0, columnspan=3, sticky=tk.EW, pady=(6, 0))

        # ── Write FTP profile ───────────────────────────────────────────
        write_f = ttk.LabelFrame(w, text="Write FTP Profile (0x90EE)", padding=8)
        write_f.pack(fill=tk.X, padx=8, pady=(0, 8))

        self._fields: dict[str, tk.StringVar] = {}
        field_specs = [
            ("profile_name", "Profile name:", "", None),
            ("ssid_24ghz",   "SSID (2.4GHz):", "", None),
            ("ssid_5ghz",    "SSID (5GHz):", "", None),
            ("host",         "FTP host:", "", None),
            ("port",         "Port:", "21", None),
            ("username",     "Username:", "", None),
            ("password",     "Password:", "", "*"),
        ]
        for row, (key, label, default, show) in enumerate(field_specs):
            ttk.Label(write_f, text=label).grid(row=row, column=0, sticky=tk.W, pady=2)
            var = tk.StringVar(value=default)
            self._fields[key] = var
            entry_kwargs = {"show": show} if show else {}
            ttk.Entry(write_f, textvariable=var, width=30, **entry_kwargs).grid(
                row=row, column=1, sticky=tk.W, padx=4, pady=2)

        btn_row = len(field_specs)
        bf = ttk.Frame(write_f)
        bf.grid(row=btn_row, column=0, columnspan=2, pady=(8, 0), sticky=tk.W)
        ttk.Button(bf, text="Preview (dry run)", command=lambda: self._do_write(dry_run=True)).pack(
            side=tk.LEFT, padx=(0, 4))
        ttk.Button(bf, text="Write to Camera…", command=lambda: self._do_write(dry_run=False)).pack(side=tk.LEFT)

        self._write_box = _make_output_box(write_f, height=4)
        self._write_box.grid(row=btn_row + 1, column=0, columnspan=2, sticky=tk.EW, pady=(6, 0))

    def _do_read(self) -> None:
        try:
            code = parse_propcode(self._code_var.get())
        except ValueError as e:
            _set_output(self._read_box, f"Invalid property code: {e}")
            return
        args = ["vendor-read", f"0x{code:04x}"]
        serial = self.app._selected_serial()
        if serial:
            args += ["--serial", serial]
        try:
            out = self.app._run(*args)
            _set_output(self._read_box, out.strip())
        except RuntimeError as e:
            _set_output(self._read_box, f"Error: {e}")

    def _read_write_fields(self) -> dict[str, str] | None:
        """Collect and validate the FTP-profile form fields. Returns None
        (with an error already shown in the output box) if invalid."""
        try:
            port = int(self._fields["port"].get().strip())
            if not is_valid_u16(port):
                raise ValueError("out of range 0-65535")
        except ValueError as e:
            _set_output(self._write_box, f"Invalid port: {e}")
            return None

        # Keep this field list (which fields, trimmed vs not) in sync with
        # the egui GUI's call to ptp_usb::ftp_profile_missing_fields (Rust,
        # gui/src/main.rs) -- Python can't call into that crate directly, so
        # this is a hand-copy of the same required-field list, not a shared
        # definition.
        values = {
            "profile_name": self._fields["profile_name"].get().strip(),
            "ssid_24ghz": self._fields["ssid_24ghz"].get().strip(),
            "ssid_5ghz": self._fields["ssid_5ghz"].get().strip(),
            "host": self._fields["host"].get().strip(),
            "port": str(port),
            "username": self._fields["username"].get().strip(),
            "password": self._fields["password"].get(),  # not stripped — a leading/trailing space could be intentional
        }
        missing = [k for k, v in values.items() if not v]
        if missing:
            _set_output(self._write_box, f"Missing required field(s): {', '.join(missing)}")
            return None
        return values

    def _do_write(self, dry_run: bool) -> None:
        values = self._read_write_fields()
        if values is None:
            return

        if not dry_run and not messagebox.askyesno(
            "Write FTP Profile",
            "This overwrites the FTP profile stored on the camera:\n\n"
            f"  Name: {values['profile_name']}\n"
            f"  Host: {values['host']}:{values['port']}\n"
            f"  User: {values['username']}\n\n"
            "Continue?",
            parent=self._window,
        ):
            return

        args = [
            "vendor-write-ftp",
            "--profile-name", values["profile_name"],
            "--ssid-24ghz", values["ssid_24ghz"],
            "--ssid-5ghz", values["ssid_5ghz"],
            "--host", values["host"],
            "--port", values["port"],
            "--username", values["username"],
            # Piped via stdin, not passed as a plain argument — the CLI's
            # own process-list (`ps`/`/proc/<pid>/cmdline`) would otherwise
            # make the password visible to any other local user while this
            # subprocess runs.
            "--password-stdin",
        ]
        serial = self.app._selected_serial()
        if serial:
            args += ["--serial", serial]
        if dry_run:
            args.append("--dry-run")

        try:
            out = self.app._run(*args, stdin_data=values["password"])
            _set_output(self._write_box, out.strip())
        except RuntimeError as e:
            _set_output(self._write_box, f"Error: {e}")


# ── Entry point ────────────────────────────────────────────────────────────

if __name__ == "__main__":
    root = tk.Tk()
    FleetApp(root)
    root.mainloop()
