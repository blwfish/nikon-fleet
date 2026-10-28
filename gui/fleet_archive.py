"""Export and import of the data directory as a zip. No tkinter.

One rule for both directions: a file travels exactly when `accept_zip_entry`
accepts its archive path and its content passes the same validity check
(`snapshot_bytes_problem` for snapshots and references, `firmware_meta_problem`
plus the recorded size and SHA-256 for a firmware entry). Export writes only
what import would accept, and counts what it left out; import never overwrites
an existing file, writes each file whole or not at all, and reports every
count: imported, skipped because it already exists, rejected, failed.
"""
import errno
import hashlib
import json
import os
import tempfile
import zipfile
import zlib
from dataclasses import dataclass, field
from pathlib import Path

from fleet_lib import (
    FIRMWARE_ENTRY_FILES,
    accept_zip_entry,
    firmware_meta_problem,
    is_plain_name_component,
    reference_file_name,
    snapshot_bytes_problem,
)

# Limits on what import will read from a zip it did not make: a JSON file, a
# firmware page, a firmware binary (Nikon's are a few hundred MB), the whole
# import, and the entry count. A body that would exceed one is rejected and
# counted, not truncated.
MAX_JSON_ENTRY_BYTES = 64 << 20
MAX_PAGE_ENTRY_BYTES = 16 << 20
MAX_BIN_ENTRY_BYTES = 1 << 30
MAX_TOTAL_BYTES = 8 << 30
MAX_ENTRIES = 100_000
_CHUNK = 1 << 20

# What reading one zip entry can raise besides OSError: a bad CRC or header, a
# corrupt deflate stream, a truncated one, an unsupported method, a password.
_ENTRY_ERRORS = (zipfile.BadZipFile, zlib.error, EOFError, NotImplementedError, RuntimeError)

KINDS = ("snapshots", "references", "firmware")


def entry_limit(name: str) -> int:
    """The most bytes import will take for one entry of this (accepted) path."""
    last = name.rsplit("/", 1)[-1]
    if last == "firmware.bin":
        return MAX_BIN_ENTRY_BYTES
    if last == "source-page.html":
        return MAX_PAGE_ENTRY_BYTES
    return MAX_JSON_ENTRY_BYTES


def _sha256_file(path: Path) -> "tuple[int, str]":
    """(size, hex digest) of a file, read in chunks."""
    h = hashlib.sha256()
    size = 0
    with open(path, "rb") as f:
        while True:
            chunk = f.read(_CHUNK)
            if not chunk:
                break
            size += len(chunk)
            h.update(chunk)
    return size, h.hexdigest()


def _unit_problem(meta_bytes: bytes, slug: str, version: str, bin_size: int, bin_sha256: str) -> "str | None":
    """Why a firmware entry (metadata plus the binary's measured size and
    digest) is not a sound archive entry, or None. The same check on both
    sides of the zip."""
    problem = firmware_meta_problem(meta_bytes, slug, version)
    if problem:
        return problem
    meta = json.loads(meta_bytes.decode("utf-8"))
    if bin_size != meta["bin_size_bytes"]:
        return f"firmware.bin is {bin_size} bytes but metadata.json records {meta['bin_size_bytes']}"
    if bin_sha256.lower() != meta["bin_sha256"].lower():
        return "firmware.bin does not match the SHA-256 recorded in metadata.json"
    return None


def _reference_problem(name: str, raw: bytes) -> "str | None":
    """Why `references/<name>.json` is not the reference file of the camera
    its own content names, or None. The CLI looks a reference up as
    `{model_slug}_{serial}.json` of the snapshot's own model and serial
    (ops::paths::checked_reference_filename); a file under any other name is
    never found, and one whose model or serial is not a plain name is refused
    by `ref set`. Takes the bytes of an already validated snapshot."""
    camera = json.loads(raw.decode("utf-8"))["camera"]
    expected = reference_file_name(camera["model"], camera["serial"])
    if expected is None:
        return "the model or serial in the file is not a plain file-name component"
    if name != f"references/{expected}":
        return f"the file's own camera ({camera['model']}, {camera['serial']}) is referenced as {expected}"
    return None


# ── Export ─────────────────────────────────────────────────────────────────────

@dataclass
class ExportReport:
    written: dict = field(default_factory=lambda: {k: 0 for k in KINDS})
    skipped: list = field(default_factory=list)      # (path, reason): in the data dir, not in the zip
    fatal: "str | None" = None                       # the zip was not produced

    def summary(self) -> str:
        if self.fatal:
            return f"Export failed: {self.fatal}. The previous export at that name is unchanged."
        total = sum(self.written.values())
        text = (f"Exported {total} file(s): {self.written['snapshots']} snapshot(s), "
                f"{self.written['references']} reference(s), {self.written['firmware']} firmware file(s).")
        if self.skipped:
            text += f" Left out {len(self.skipped)} item(s):\n" + _bullets(self.skipped)
        return text


def _bullets(pairs, limit: int = 8) -> str:
    lines = [f"  {name}: {why}" for name, why in pairs[:limit]]
    if len(pairs) > limit:
        lines.append(f"  … and {len(pairs) - limit} more")
    return "\n".join(lines)


def _plan_export(data_dir: Path, report: ExportReport) -> list:
    """[(arcname, path, compress_type)] for everything that passes the import
    rule; the rest is added to report.skipped with the reason."""
    plan = []

    def listdir(d: Path, label: str):
        try:
            return sorted(d.iterdir())
        except OSError as e:
            report.skipped.append((label, f"directory could not be read: {e}"))
            return []

    for folder in ("snapshots", "references"):
        d = data_dir / folder
        if not d.is_dir():
            continue
        for p in listdir(d, folder):
            name = f"{folder}/{p.name}"
            if p.is_symlink() or not p.is_file():
                report.skipped.append((name, "not a regular file"))
            elif not accept_zip_entry(name):
                report.skipped.append((name, "not a .json file with a plain name"))
            else:
                try:
                    if p.stat().st_size > MAX_JSON_ENTRY_BYTES:
                        report.skipped.append((name, f"over the {MAX_JSON_ENTRY_BYTES >> 20} MiB import limit"))
                        continue
                    problem = snapshot_bytes_problem(p.read_bytes())
                except OSError as e:
                    report.skipped.append((name, f"could not be read: {e}"))
                    continue
                if problem:
                    report.skipped.append((name, f"not a valid snapshot ({problem[0]}: {problem[1]})"))
                    continue
                if folder == "references":
                    why = _reference_problem(name, p.read_bytes())
                    if why:
                        report.skipped.append((name, why))
                        continue
                plan.append((name, p, zipfile.ZIP_DEFLATED))

    fw = data_dir / "firmware"
    if not fw.is_dir():
        return plan
    for model_dir in listdir(fw, "firmware"):
        mname = f"firmware/{model_dir.name}"
        if model_dir.is_symlink() or not model_dir.is_dir():
            report.skipped.append((mname, "not an archive directory (the legacy flat layout is not archive content)"))
            continue
        for ver_dir in listdir(model_dir, mname):
            plan.extend(_plan_firmware_entry(ver_dir, f"{mname}/{ver_dir.name}", model_dir.name, report))
    return plan


def _plan_firmware_entry(ver_dir: Path, label: str, slug: str, report: ExportReport) -> list:
    if ver_dir.is_symlink() or not ver_dir.is_dir():
        report.skipped.append((label, "not an archive directory"))
        return []
    try:
        children = sorted(ver_dir.iterdir())
    except OSError as e:
        report.skipped.append((label, f"directory could not be read: {e}"))
        return []
    files = {}
    for c in children:
        if c.name in FIRMWARE_ENTRY_FILES and c.is_file() and not c.is_symlink():
            files[c.name] = c
        else:
            report.skipped.append((f"{label}/{c.name}", "not part of a firmware archive entry"))
    if "metadata.json" not in files and "firmware.bin" not in files:
        return []
    if "metadata.json" not in files or "firmware.bin" not in files:
        report.skipped.append((label, "incomplete entry: needs both firmware.bin and metadata.json"))
        return []
    try:
        if files["firmware.bin"].stat().st_size > MAX_BIN_ENTRY_BYTES:
            report.skipped.append((label, f"firmware.bin is over the {MAX_BIN_ENTRY_BYTES >> 20} MiB import limit"))
            return []
        size, digest = _sha256_file(files["firmware.bin"])
        problem = _unit_problem(files["metadata.json"].read_bytes(), slug, ver_dir.name, size, digest)
    except OSError as e:
        report.skipped.append((label, f"could not be read: {e}"))
        return []
    if problem:
        report.skipped.append((label, problem))
        return []
    return [(f"{label}/{n}", p, zipfile.ZIP_STORED if n == "firmware.bin" else zipfile.ZIP_DEFLATED)
            for n, p in sorted(files.items())]


def export_archive(data_dir: Path, dest: Path) -> ExportReport:
    """Write the data directory's archive content to *dest* (a zip).

    Built in a temporary file beside *dest* and renamed over it only when
    complete, so a failure part-way leaves any previous export untouched.
    """
    data_dir, dest = Path(data_dir), Path(dest)
    report = ExportReport()
    plan = _plan_export(data_dir, report)
    tmp = None
    try:
        fd, tmp = tempfile.mkstemp(prefix=f".{dest.name}.", suffix=".part", dir=dest.parent)
        # strict_timestamps=False: a file dated before 1980 is stored as 1980, not a failed export
        with os.fdopen(fd, "wb") as f, zipfile.ZipFile(f, "w", zipfile.ZIP_DEFLATED, strict_timestamps=False) as zf:
            for arcname, path, compress in plan:
                zf.write(path, arcname, compress_type=compress)
        os.replace(tmp, dest)
        tmp = None
    except (OSError, zipfile.BadZipFile, zlib.error) as e:
        report.fatal = f"{type(e).__name__}: {e}"
        return report
    finally:
        if tmp is not None:
            try:
                os.unlink(tmp)
            except OSError as e:
                report.skipped.append((tmp, f"temporary file could not be removed: {e}"))
    for arcname, _path, _compress in plan:
        report.written[arcname.split("/", 1)[0]] += 1
    return report


# ── Import ─────────────────────────────────────────────────────────────────────

@dataclass
class ImportReport:
    imported: dict = field(default_factory=lambda: {k: 0 for k in KINDS})
    skipped_existing: list = field(default_factory=list)   # names that were already there, left alone
    rejected: list = field(default_factory=list)           # (name, reason): not taken
    errors: list = field(default_factory=list)             # (name, reason): taking it failed
    ignored_directories: int = 0
    fatal: "str | None" = None                             # the archive could not be read at all
    stopped: "str | None" = None                           # import ended early, with this reason

    def summary(self) -> str:
        if self.fatal:
            return f"Import failed: {self.fatal}. Nothing was imported."
        text = (f"Imported {self.imported['snapshots']} snapshot(s), {self.imported['references']} "
                f"reference(s), {self.imported['firmware']} firmware file(s); "
                f"skipped {len(self.skipped_existing)} already present; "
                f"rejected {len(self.rejected)}; {len(self.errors)} failed.")
        if self.stopped:
            text += f"\nImport stopped early: {self.stopped}"
        if self.skipped_existing:
            text += "\nAlready present (not overwritten):\n" + _bullets([(n, "exists") for n in self.skipped_existing])
        if self.rejected:
            text += "\nRejected:\n" + _bullets(self.rejected)
        if self.errors:
            text += "\nFailed:\n" + _bullets(self.errors)
        return text


class _TooLarge(Exception):
    pass


def _read_capped(zf: zipfile.ZipFile, info: zipfile.ZipInfo, limit: int) -> bytes:
    """An entry's bytes, refusing more than *limit* whatever the header says."""
    with zf.open(info) as f:
        data = f.read(limit + 1)
    if len(data) > limit:
        raise _TooLarge()
    return data


# Names the import's own temporary files carry. mkstemp makes each one unique,
# so no file this import did not create is ever opened, renamed over or deleted.
_TMP_PREFIX = ".fleet-import-"
# link(2) refused for a reason other than "it exists": a filesystem without hard links.
_NO_HARDLINKS = (errno.EPERM, errno.ENOTSUP, errno.ENOSYS, errno.EMLINK, errno.EOPNOTSUPP)


def _discard(path: Path) -> None:
    """Best-effort removal of one of our own temporary files (a leftover is a
    hidden `.fleet-import-*.tmp` the CLI never lists)."""
    try:
        os.unlink(path)
    except OSError:
        pass


def _stage(directory: Path, chunks) -> Path:
    """Write *chunks* to a new uniquely named temporary file in *directory*,
    flushed and fsynced, and return its path. On any failure (the write, the
    close and the flush included) the temporary file is removed."""
    fd, name = tempfile.mkstemp(dir=directory, prefix=_TMP_PREFIX, suffix=".tmp")
    tmp = Path(name)
    try:
        with os.fdopen(fd, "wb") as f:
            for chunk in chunks:
                f.write(chunk)
            f.flush()
            os.fsync(f.fileno())
            os.fchmod(f.fileno(), 0o644)
    except BaseException:
        _discard(tmp)
        raise
    return tmp


def _publish(tmp: Path, dest: Path) -> None:
    """Give the finished temporary file *tmp* the name *dest* without ever
    replacing anything: a hard link fails if *dest* exists (a symlink, dangling
    or not, included), then the temporary name is removed. *dest* therefore
    either does not exist or is the whole file. FileExistsError if it exists.
    *tmp* is always gone afterwards."""
    try:
        try:
            os.link(tmp, dest)
        except OSError as e:
            if isinstance(e, FileExistsError) or e.errno not in _NO_HARDLINKS:
                raise
            # No hard links on this filesystem: a rename is still atomic, with a
            # (small) window between the check and the rename.
            if os.path.lexists(dest):
                raise FileExistsError(errno.EEXIST, "already exists", str(dest)) from None
            os.rename(tmp, dest)
    finally:
        _discard(tmp)


def _write_new(path: Path, data: bytes) -> None:
    """Create *path* holding all of *data*, or not at all, and never replace
    anything: staged, fsynced, then published. FileExistsError if it exists."""
    _publish(_stage(path.parent, [data]), path)


def _exists(path: Path) -> bool:
    return path.exists() or path.is_symlink()


def import_archive(src: Path, data_dir: Path) -> ImportReport:
    """Import the snapshots, references and firmware entries of the zip at *src*."""
    data_dir = Path(data_dir)
    report = ImportReport()
    try:
        zf = zipfile.ZipFile(src)
    except (zipfile.BadZipFile, OSError) as e:
        report.fatal = f"{src} could not be read as a zip: {e}"
        return report
    with zf:
        _import_entries(zf, data_dir, report)
    return report


def _import_entries(zf: zipfile.ZipFile, data_dir: Path, report: ImportReport) -> None:
    infos = zf.infolist()
    if len(infos) > MAX_ENTRIES:
        report.fatal = f"the archive has {len(infos)} entries (limit {MAX_ENTRIES})"
        return
    seen = set()
    units: dict = {}
    total = 0
    for info in infos:
        name = info.filename
        if info.is_dir() or name.endswith("\\"):
            # a directory entry (zip tools write them with a trailing slash)
            report.ignored_directories += 1
            continue
        if name in seen:
            report.rejected.append((name, "duplicate entry name"))
            continue
        seen.add(name)
        if not accept_zip_entry(name):
            report.rejected.append((name, "not a snapshots/, references/ or firmware/ archive path"))
            continue
        if info.flag_bits & 0x1:
            report.rejected.append((name, "encrypted"))
            continue
        if info.file_size > entry_limit(name):
            report.rejected.append((name, f"{info.file_size} bytes is over the {entry_limit(name)}-byte limit"))
            continue
        parts = name.split("/")
        if parts[0] == "firmware":
            units.setdefault((parts[1], parts[2]), {})[parts[3]] = info
            continue
        try:
            raw = _read_capped(zf, info, entry_limit(name))
            total += len(raw)
            if total > MAX_TOTAL_BYTES:
                report.stopped = f"more than {MAX_TOTAL_BYTES >> 30} GiB in total"
                return
            _import_json(name, parts[0], raw, data_dir, report)
        except _TooLarge:
            report.rejected.append((name, "larger than its header says; over the limit"))
        except (OSError, *_ENTRY_ERRORS) as e:
            report.errors.append((name, f"{type(e).__name__}: {e}"))
    for (slug, version), files in sorted(units.items()):
        try:
            total += _import_firmware_entry(zf, slug, version, files, data_dir, report)
        except _TooLarge:
            report.rejected.append((f"firmware/{slug}/{version}", "larger than its header says; over the limit"))
        except (OSError, *_ENTRY_ERRORS) as e:
            report.errors.append((f"firmware/{slug}/{version}", f"{type(e).__name__}: {e}"))
        if total > MAX_TOTAL_BYTES:
            report.stopped = f"more than {MAX_TOTAL_BYTES >> 30} GiB in total"
            return


def _import_json(name: str, kind: str, raw: bytes, data_dir: Path, report: ImportReport) -> None:
    problem = snapshot_bytes_problem(raw)
    if problem:
        report.rejected.append((name, f"not a valid snapshot ({problem[0]}: {problem[1]})"))
        return
    if kind == "references":
        why = _reference_problem(name, raw)
        if why:
            report.rejected.append((name, why))
            return
    dest = data_dir / name
    if _exists(dest):
        report.skipped_existing.append(name)
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    try:
        _write_new(dest, raw)
    except FileExistsError:
        report.skipped_existing.append(name)
        return
    report.imported[kind] += 1


def _make_dirs(path: Path) -> list:
    """mkdir -p, returning exactly the directories this call created (top first)."""
    missing = []
    p = path
    while not p.exists() and p != p.parent:
        missing.append(p)
        p = p.parent
    path.mkdir(parents=True, exist_ok=True)
    return list(reversed(missing))


def _import_firmware_entry(zf, slug: str, version: str, files: dict, data_dir: Path, report: ImportReport) -> int:
    """Import one firmware entry as a unit; returns the bytes written.

    Only paths this import created are ever removed: the binary is staged under
    a unique temporary name (never `firmware.bin.part`, which a running
    `firmware browse` may own) and published without replacing anything, and
    an entry whose directory already holds any of its files, or a `.part`, is
    left entirely alone."""
    label = f"firmware/{slug}/{version}"
    if "metadata.json" not in files or "firmware.bin" not in files:
        missing = "metadata.json" if "metadata.json" not in files else "firmware.bin"
        report.rejected.extend((f"{label}/{n}", f"incomplete entry: no {missing}") for n in sorted(files))
        return 0
    meta_raw = _read_capped(zf, files["metadata.json"], MAX_JSON_ENTRY_BYTES)
    dest_dir = data_dir / "firmware" / slug / version
    if any(_exists(dest_dir / n) for n in (*FIRMWARE_ENTRY_FILES, "firmware.bin.part")):
        report.skipped_existing.extend(f"{label}/{n}" for n in sorted(files))
        return 0
    created_dirs = _make_dirs(dest_dir)
    made: list = []
    written = 0
    try:
        digest = hashlib.sha256()
        seen = [0]

        def chunks():
            with zf.open(files["firmware.bin"]) as src:
                while True:
                    chunk = src.read(_CHUNK)
                    if not chunk:
                        return
                    seen[0] += len(chunk)
                    if seen[0] > MAX_BIN_ENTRY_BYTES:
                        raise _TooLarge()
                    digest.update(chunk)
                    yield chunk

        staged = _stage(dest_dir, chunks())
        problem = _unit_problem(meta_raw, slug, version, seen[0], digest.hexdigest())
        if problem:
            _discard(staged)
            report.rejected.extend((f"{label}/{n}", problem) for n in sorted(files))
            _undo([], created_dirs, report)
            return 0
        _publish(staged, dest_dir / "firmware.bin")
        made.append(dest_dir / "firmware.bin")
        written += seen[0]
        if "source-page.html" in files:
            page = _read_capped(zf, files["source-page.html"], MAX_PAGE_ENTRY_BYTES)
            _publish(_stage(dest_dir, [page]), dest_dir / "source-page.html")
            made.append(dest_dir / "source-page.html")
            written += len(page)
        # Last, as in the CLI: a directory without metadata.json is an
        # incomplete entry, so it appears only when everything else is in place.
        _publish(_stage(dest_dir, [meta_raw]), dest_dir / "metadata.json")
        made.append(dest_dir / "metadata.json")
        written += len(meta_raw)
    except FileExistsError:
        # Something created one of these names after the check above. It is not ours:
        # leave it, take back what we made, and report the entry as present.
        _undo(made, created_dirs, report)
        report.skipped_existing.extend(f"{label}/{n}" for n in sorted(files))
        return 0
    except BaseException:
        _undo(made, created_dirs, report)
        raise
    if not made:
        _undo([], created_dirs, report)
    report.imported["firmware"] += len(made)
    return written


def _undo(made: list, created_dirs: list, report: ImportReport) -> None:
    """Remove the files this import published and the directories it created
    (an `rmdir` that fails because someone else put something there is right to fail)."""
    for p in made:
        try:
            p.unlink()
        except FileNotFoundError:
            pass
        except OSError as e:
            report.errors.append((str(p), f"left behind after a failed import: {e}"))
    for d in reversed(created_dirs):
        try:
            d.rmdir()
        except OSError:
            pass


# ── Removing a firmware entry ──────────────────────────────────────────────────

def firmware_entry_path(root: Path, directory: str) -> Path:
    """*directory* (as `firmware ls --json` reported it) as a path, only if it
    is exactly `<root>/<model dir>/<version dir>`: two plain components below
    the firmware root, each a real directory (not a link). Anything else could
    be another archive or somewhere else entirely. Raises ValueError."""
    root = Path(root)
    path = Path(directory)
    try:
        rel = path.relative_to(root)
    except ValueError:
        raise ValueError(f"{directory} is not under the firmware directory {root}") from None
    if len(rel.parts) != 2 or not all(is_plain_name_component(p) for p in rel.parts):
        raise ValueError(f"{directory} is not a <model>/<version> directory of {root}")
    current = root
    for part in rel.parts:
        current = current / part
        if current.is_symlink() or not current.is_dir():
            raise ValueError(f"{current} is not a real directory")
    real_root = Path(os.path.realpath(root))
    if Path(os.path.realpath(path)) != real_root / rel:
        raise ValueError(f"{directory} does not resolve to {real_root / rel}")
    return path


def remove_firmware_entry(root: Path, directory: str) -> None:
    """Delete one firmware archive entry: the files an entry consists of,
    then its directory (and the model's, if that is now empty). A directory
    that still holds anything else is left, and OSError says so: nothing here
    deletes a tree. Raises ValueError for a path that is not an entry."""
    path = firmware_entry_path(root, directory)
    for name in (*FIRMWARE_ENTRY_FILES, "firmware.bin.part"):
        (path / name).unlink(missing_ok=True)
    path.rmdir()
    try:
        path.parent.rmdir()
    except OSError:
        pass  # other versions of this model are still there
