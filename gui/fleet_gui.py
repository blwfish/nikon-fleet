#!/usr/bin/env python3
"""Fleet GUI — Nikon camera fleet manager (tkinter / subprocess frontend)."""

import json
import shutil
import subprocess
import zipfile
from pathlib import Path
import tkinter as tk
from tkinter import ttk, messagebox, filedialog

# ── Paths ──────────────────────────────────────────────────────────────────

_PROJECT = Path(__file__).resolve().parent.parent   # repo root

def _fleet_bin() -> Path:
    for p in [_PROJECT / "target" / "release" / "fleet",
              _PROJECT / "target" / "debug"   / "fleet"]:
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
        self._status("Click Discover to find cameras.")

    # ── UI ──────────────────────────────────────────────────────────────

    def _build_ui(self) -> None:
        bar = ttk.Frame(self.root, padding=4)
        bar.pack(fill=tk.X)
        ttk.Button(bar, text="⟳  Discover",      command=self.discover).pack(side=tk.LEFT)
        ttk.Separator(bar, orient=tk.VERTICAL).pack(side=tk.LEFT, fill=tk.Y, padx=4)
        ttk.Label(bar, text="Label:").pack(side=tk.LEFT)
        self._label = tk.StringVar()
        ttk.Entry(bar, textvariable=self._label, width=18).pack(side=tk.LEFT, padx=2)
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

        # Snapshot table
        snap_f = ttk.LabelFrame(pw, text="Snapshots", padding=4)
        self._tree = ttk.Treeview(snap_f, columns=("ts", "label", "ref"),
                                    show="headings", selectmode="browse")
        self._tree.heading("ts",    text="Captured")
        self._tree.heading("label", text="Label")
        self._tree.heading("ref",   text="")
        self._tree.column("ts",    width=175, stretch=False)
        self._tree.column("label", width=200)
        self._tree.column("ref",   width=55,  stretch=False)
        sb = ttk.Scrollbar(snap_f, orient=tk.VERTICAL, command=self._tree.yview)
        self._tree.configure(yscrollcommand=sb.set)
        self._tree.pack(side=tk.LEFT, fill=tk.BOTH, expand=True)
        sb.pack(side=tk.RIGHT, fill=tk.Y)
        ttk.Button(snap_f, text="Set as Reference",
                   command=self.set_reference).pack(side=tk.BOTTOM, anchor=tk.W, pady=4)
        pw.add(snap_f, weight=3)

    def _status(self, msg: str) -> None:
        self._status_var.set(msg)
        self.root.update_idletasks()

    # ── CLI bridge ──────────────────────────────────────────────────────

    def _run(self, *args: str) -> str:
        cmd = [str(_fleet_bin()), "--data-dir", str(_data_dir())] + list(args)
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
            self.cameras = json.loads(out)["cameras"]
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
        dd       = _data_dir()
        snap_dir = dd / "snapshots"
        ref_dir  = dd / "references"
        refs     = {f.name for f in ref_dir.iterdir()} if ref_dir.exists() else set()
        if not snap_dir.exists():
            return
        rows = []
        for f in snap_dir.glob("*.json"):
            try:
                d = json.loads(f.read_text())
                if d["camera"]["serial"] == serial:
                    rows.append((d.get("captured_at", "")[:19],
                                 d.get("label") or "", f.name))
            except Exception:
                pass
        rows.sort(reverse=True)
        for ts, label, fname in rows:
            ref_mark = "◀ ref" if fname in refs else ""
            self._tree.insert("", tk.END, iid=fname, values=(ts, label, ref_mark))

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
        shutil.copy2(dd / "snapshots" / fname, ref_dir / fname)
        self._load_snapshots()
        self._status("Reference set.")

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
        self._status(f"Exported {count} file(s) → {Path(dest).name}")

    def import_data(self) -> None:
        src = filedialog.askopenfilename(
            filetypes=[("Zip archive", "*.zip")],
            parent=self.root,
        )
        if not src:
            return
        dd = _data_dir()
        snaps = refs = 0
        with zipfile.ZipFile(src) as zf:
            for name in zf.namelist():
                parts = Path(name).parts
                if len(parts) == 2 and parts[0] in ("snapshots", "references") \
                        and name.endswith(".json"):
                    out = dd / parts[0] / parts[1]
                    out.parent.mkdir(parents=True, exist_ok=True)
                    out.write_bytes(zf.read(name))
                    if parts[0] == "snapshots": snaps += 1
                    else:                        refs  += 1
        self._load_snapshots()
        self._status(f"Imported {snaps} snapshot(s), {refs} reference(s).")

# ── Entry point ────────────────────────────────────────────────────────────

if __name__ == "__main__":
    root = tk.Tk()
    FleetApp(root)
    root.mainloop()
