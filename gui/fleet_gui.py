#!/usr/bin/env python3
"""Fleet GUI — Nikon camera fleet manager (tkinter / subprocess frontend)."""

import json
import shutil
import subprocess
import zipfile
from pathlib import Path
import tkinter as tk
from tkinter import ttk, messagebox, filedialog

from fleet_lib import strip_sdk_prefix, accept_zip_entry, parse_fw_filename

# ── Paths ──────────────────────────────────────────────────────────────────

_PROJECT = Path(__file__).resolve().parent.parent   # repo root

def _fleet_bin() -> Path:
    for p in [_PROJECT / "target" / "release" / "nikon-fleet",
              _PROJECT / "target" / "debug"   / "nikon-fleet"]:
        if p.exists():
            return p
    raise RuntimeError(
        "fleet binary not found.\n"
        "Run `cargo build --release` in the project root first."
    )

def _data_dir() -> Path:
    cfg = Path.home() / "Library" / "Preferences" / "net.blw.fleet" / "settings.json"
    try:
        s = json.loads(cfg.read_text())
        if s.get("data_dir"):
            return Path(s["data_dir"])
    except Exception:
        pass
    return Path.home() / "Library" / "Application Support" / "net.blw.fleet"

def _firmware_dir() -> Path:
    return _data_dir() / "firmware"

def _save_data_dir(new_dir: str) -> None:
    cfg_dir = Path.home() / "Library" / "Preferences" / "net.blw.fleet"
    cfg_dir.mkdir(parents=True, exist_ok=True)
    cfg = cfg_dir / "settings.json"
    try:
        s = json.loads(cfg.read_text())
    except Exception:
        s = {}
    s["data_dir"] = new_dir or None
    cfg.write_text(json.dumps(s, indent=2))

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
            except Exception:
                pass
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

    def _run(self, *args: str) -> str:
        cmd = [str(_fleet_bin()), "--no-usb-reset", "--data-dir", str(_data_dir())] + list(args)
        r = subprocess.run(cmd, capture_output=True, text=True)
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
            ref_name = f"{model.replace(' ', '_')}_{serial}.json"
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
            except Exception:
                pass
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
            ref_fname = f"{cam['model'].replace(' ', '_')}_{cam['serial']}.json"
            shutil.copy2(dd / "snapshots" / fname, ref_dir / ref_fname)
        except OSError as e:
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
        fw_dir = _firmware_dir()
        if not fw_dir.exists():
            return
        # Nested archive layout: firmware/{model_slug}/{version}/metadata.json
        for meta_file in sorted(fw_dir.rglob("metadata.json")):
            try:
                meta = json.loads(meta_file.read_text())
                model   = meta.get("model", "")
                version = meta.get("firmware_version", "")
                slug    = meta_file.parent.parent.name
                ver     = meta_file.parent.name
                iid     = f"nested|{slug}|{ver}"
                self._fw_tree.insert("", tk.END, iid=iid,
                                     values=(model, version, f"{slug}/{ver}/firmware.bin"))
            except Exception:
                continue
        # Legacy flat layout: firmware/*.bin (added via old GUI Add button)
        for f in sorted(fw_dir.glob("*.bin")):
            model, version = parse_fw_filename(f.name)
            self._fw_tree.insert("", tk.END, iid=f"flat|{f.name}",
                                 values=(model, version, f.name))

    def add_firmware(self) -> None:
        src = filedialog.askopenfilename(
            title="Add firmware to library",
            filetypes=[("Nikon firmware", "*.bin"), ("All files", "*.*")],
            parent=self.root,
        )
        if not src:
            return
        fw_dir = _firmware_dir()
        fw_dir.mkdir(parents=True, exist_ok=True)
        dest = fw_dir / Path(src).name
        if dest.exists():
            if not messagebox.askyesno("Already exists",
                                       f"{dest.name} is already in the library. Overwrite?",
                                       parent=self.root):
                return
        try:
            shutil.copy2(src, dest)
        except OSError as e:
            messagebox.showerror("Add firmware failed", str(e), parent=self.root)
            return
        self._load_firmware()
        self._status(f"Added {dest.name}.")

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

# ── Entry point ────────────────────────────────────────────────────────────

if __name__ == "__main__":
    root = tk.Tk()
    FleetApp(root)
    root.mainloop()
