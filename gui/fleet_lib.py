"""Pure helper functions — no tkinter, no subprocess. Importable for testing.

This module is the GUI's half of the contract with the CLI: it builds argv, and
it parses what the CLI's ``--json`` modes print. It does not re-implement the
CLI's rules. Where the GUI must still read a file itself (the snapshot detail
window, import/export validation) the rule is pinned to the shared fixture in
``tests/fixtures/snapshot_validity``.
"""
import json
from dataclasses import dataclass, field
from pathlib import Path

# `EXIT_ALREADY_ARCHIVED` in src/main.rs, documented in `firmware add --help`
# ("Exit status:"): the refusal the GUI turns into an "Overwrite?" prompt. The
# GUI tests the exit status, never the CLI's message text.
EXIT_ALREADY_ARCHIVED = 3

# `EXIT_WRITE_INDETERMINATE` in src/main.rs, documented in `vendor-write-ftp
# --help` ("Exit status:"): the profile data was sent and the write then
# failed, so the camera MAY have changed. Exit 1 there is a definitive failure.
EXIT_WRITE_INDETERMINATE = 4

# `CURRENT_FORMAT_VERSION` (src/snapshot.rs) and `FIRMWARE_META_FORMAT_VERSION`
# (src/firmware.rs). Pinned by the shared fixtures, not by a comment.
SNAPSHOT_FORMAT_VERSION = 1
FIRMWARE_META_FORMAT_VERSION = 1


class CliOutputError(ValueError):
    """The CLI's stdout was not the document the GUI asked for."""


# ── Reading the CLI's JSON ─────────────────────────────────────────────────────

def parse_json_stdout(out: str, what: str = "output"):
    """Parse the CLI's whole stdout as one JSON document.

    Every ``--json`` mode keeps stdout a single document (the SDK's banner
    text is kept off it), so anything else is an error to show, not something
    to dig a ``{`` out of. Raises CliOutputError naming *what* was expected.
    """
    try:
        return json.loads(out)
    except ValueError as e:
        shown = out.strip()[:200]
        raise CliOutputError(
            f"the CLI's {what} is not a single JSON document ({e}); it printed: {shown!r}"
        ) from e


def _is_int(x) -> bool:
    """A JSON integer: Python's bool is an int subclass, JSON's true is not a number."""
    return isinstance(x, int) and not isinstance(x, bool)


def _expect(cond: bool, what: str, where: str) -> None:
    if not cond:
        raise CliOutputError(f"{where}: {what}")


def _expect_obj(x, where: str) -> dict:
    _expect(isinstance(x, dict), f"expected an object, got {type(x).__name__}", where)
    return x


def _expect_str(obj: dict, key: str, where: str) -> str:
    _expect(isinstance(obj.get(key), str), f"`{key}` is missing or not a string", where)
    return obj[key]


def _opt_str(obj: dict, key: str, where: str) -> str:
    """A string that may be null or absent (shown as empty)."""
    v = obj.get(key)
    _expect(v is None or isinstance(v, str), f"`{key}` is not a string or null", where)
    return v or ""


@dataclass(frozen=True)
class SnapshotRow:
    file: str
    model: str
    serial: str
    firmware: str
    label: str
    captured_at: str
    property_count: int
    is_reference: "bool | None"    # None: the CLI did not say


@dataclass(frozen=True)
class UnreadableEntry:
    file: str
    error_kind: str
    error: str


@dataclass
class SnapshotListing:
    dir: str
    dir_exists: bool
    rows: list = field(default_factory=list)
    unreadable: list = field(default_factory=list)
    skipped_dir_entries: int = 0
    reference_read_failures: int = 0


def parse_listing(stdout: str) -> SnapshotListing:
    """`nikon-fleet ls --json` -> SnapshotListing. Strict: a malformed
    document raises CliOutputError before anything is shown."""
    doc = _expect_obj(parse_json_stdout(stdout, "ls --json output"), "ls --json")
    entries = doc.get("entries")
    _expect(isinstance(entries, list), "`entries` is missing or not a list", "ls --json")
    listing = SnapshotListing(
        dir=_expect_str(doc, "dir", "ls --json"),
        dir_exists=doc.get("dir_exists") is True,
        skipped_dir_entries=doc.get("skipped_dir_entries", 0),
        reference_read_failures=doc.get("reference_read_failures", 0),
    )
    _expect(_is_int(listing.skipped_dir_entries) and _is_int(listing.reference_read_failures),
            "a count is not an integer", "ls --json")
    for i, e in enumerate(entries):
        where = f"ls --json entry {i}"
        e = _expect_obj(e, where)
        name = _expect_str(e, "file", where)
        status = e.get("status")
        if status == "ok":
            count = e.get("property_count")
            _expect(_is_int(count), "`property_count` is not an integer", where)
            ref = e.get("is_reference")
            _expect(ref is None or isinstance(ref, bool), "`is_reference` is not a boolean", where)
            listing.rows.append(SnapshotRow(
                file=name,
                model=_expect_str(e, "model", where),
                serial=_expect_str(e, "serial", where),
                firmware=_opt_str(e, "firmware", where),
                label=_opt_str(e, "label", where),
                captured_at=_opt_str(e, "captured_at", where),
                property_count=count,
                is_reference=ref,
            ))
        elif status == "unreadable":
            listing.unreadable.append(UnreadableEntry(
                file=name,
                error_kind=_expect_str(e, "error_kind", where),
                error=_opt_str(e, "error", where),
            ))
        else:
            raise CliOutputError(f"{where}: unknown status {status!r}")
    return listing


def known_cameras(listing: SnapshotListing) -> list:
    """The cameras that have readable snapshots, keyed by (model, serial): an
    `id-N` serial can be shared by two models, so the serial alone is not an
    identity. Firmware is the newest snapshot's. Sorted by model, serial."""
    newest: dict = {}
    for r in listing.rows:
        key = (r.model, r.serial)
        if key not in newest or (r.captured_at, r.file) > (newest[key].captured_at, newest[key].file):
            newest[key] = r
    return [{"model": m, "serial": s, "firmware": newest[(m, s)].firmware}
            for (m, s) in sorted(newest)]


def rows_for_camera(listing: SnapshotListing, model: str, serial: str) -> list:
    """One camera's rows, newest first by the full `captured_at` string (the
    egui GUI's order), file name breaking ties so the order is stable."""
    mine = [r for r in listing.rows if r.model == model and r.serial == serial]
    return sorted(mine, key=lambda r: (r.captured_at, r.file), reverse=True)


def listing_notes(listing: SnapshotListing) -> str:
    """What the CLI could not list, as one line ('' when everything was listed)."""
    notes = []
    if listing.unreadable:
        notes.append(f"{len(listing.unreadable)} snapshot file(s) unreadable")
    if listing.skipped_dir_entries:
        notes.append(f"{listing.skipped_dir_entries} directory entries could not be read")
    if listing.reference_read_failures:
        notes.append(f"{listing.reference_read_failures} reference file(s) could not be read "
                     "(their snapshots show no reference mark)")
    return "; ".join(notes)


def listing_ref_marks(listing: SnapshotListing) -> dict:
    """{(model, serial): [files marked as the reference]}, for the "replace the
    reference?" prompt."""
    marks: dict = {}
    for r in listing.rows:
        if r.is_reference:
            marks.setdefault((r.model, r.serial), []).append(r.file)
    return marks


@dataclass(frozen=True)
class FirmwareRow:
    model: str
    version: str
    directory: str
    bin_present: bool
    bin_size_bytes: "int | None"


@dataclass
class FirmwareListing:
    dir: str
    dir_exists: bool
    rows: list = field(default_factory=list)
    unreadable: list = field(default_factory=list)   # UnreadableEntry (file = directory)
    skipped_dir_entries: int = 0


def parse_firmware_listing(stdout: str) -> FirmwareListing:
    """`nikon-fleet firmware ls --json` -> FirmwareListing (strict)."""
    doc = _expect_obj(parse_json_stdout(stdout, "firmware ls --json output"), "firmware ls --json")
    entries = doc.get("entries")
    _expect(isinstance(entries, list), "`entries` is missing or not a list", "firmware ls --json")
    out = FirmwareListing(
        dir=_expect_str(doc, "dir", "firmware ls --json"),
        dir_exists=doc.get("dir_exists") is True,
        skipped_dir_entries=doc.get("skipped_dir_entries", 0),
    )
    _expect(_is_int(out.skipped_dir_entries), "`skipped_dir_entries` is not an integer", "firmware ls --json")
    for i, e in enumerate(entries):
        where = f"firmware ls --json entry {i}"
        e = _expect_obj(e, where)
        directory = _expect_str(e, "directory", where)
        status = e.get("status")
        if status == "ok":
            present = e.get("bin_present")
            _expect(isinstance(present, bool), "`bin_present` is missing or not a boolean", where)
            size = e.get("bin_size_bytes")
            _expect(size is None or _is_int(size), "`bin_size_bytes` is not an integer", where)
            out.rows.append(FirmwareRow(
                model=_expect_str(e, "model", where),
                version=_expect_str(e, "version", where),
                directory=directory,
                bin_present=present,
                bin_size_bytes=size,
            ))
        elif status == "unreadable":
            out.unreadable.append(UnreadableEntry(
                file=directory,
                error_kind=_expect_str(e, "error_kind", where),
                error=_opt_str(e, "error", where),
            ))
        else:
            raise CliOutputError(f"{where}: unknown status {status!r}")
    return out


def firmware_display_rows(listing: FirmwareListing) -> list:
    """Rows for the firmware table: (iid, model, version, location, status).

    The iid is the entry's directory as the CLI reported it. An entry whose
    binary is gone is shown as such, not as archived; an entry the CLI could
    not read is shown with its error kind.
    """
    rows = []
    for r in listing.rows:
        status = "ok" if r.bin_present else "BINARY MISSING"
        rows.append((r.directory, r.model, r.version, _fw_location(listing.dir, r.directory), status))
    for u in listing.unreadable:
        rows.append((u.file, "(unreadable)", "", _fw_location(listing.dir, u.file),
                     f"unreadable: {u.error_kind}"))
    return rows


def _fw_location(root: str, directory: str) -> str:
    """The entry's directory relative to the firmware root, for display."""
    try:
        return str(Path(directory).relative_to(root))
    except ValueError:
        return directory


@dataclass(frozen=True)
class SettingsInfo:
    settings_path: str
    settings_exist: bool
    data_dir: "str | None"
    effective_data_dir: str
    keys: dict


def parse_settings_show(stdout: str) -> SettingsInfo:
    """`nikon-fleet settings show --json` -> SettingsInfo (strict)."""
    doc = _expect_obj(parse_json_stdout(stdout, "settings show --json output"), "settings show --json")
    where = "settings show --json"
    effective = _expect_str(doc, "effective_data_dir", where)
    _expect(effective != "", "`effective_data_dir` is empty", where)
    data_dir = doc.get("data_dir")
    _expect(data_dir is None or isinstance(data_dir, str), "`data_dir` is not a string or null", where)
    keys = doc.get("keys", {})
    _expect(isinstance(keys, dict), "`keys` is not an object", where)
    return SettingsInfo(
        settings_path=_expect_str(doc, "settings_path", where),
        settings_exist=doc.get("settings_exist") is True,
        data_dir=data_dir,
        effective_data_dir=effective,
        keys=keys,
    )


def parse_discover(stdout: str) -> list:
    """`nikon-fleet discover --json` -> list of camera dicts. All validated
    before the caller assigns anything, so a bad document cannot leave the
    camera list half-replaced."""
    doc = _expect_obj(parse_json_stdout(stdout, "discover --json output"), "discover --json")
    cams = doc.get("cameras")
    _expect(isinstance(cams, list), "`cameras` is missing or not a list", "discover --json")
    out = []
    for i, c in enumerate(cams):
        where = f"discover --json camera {i}"
        c = _expect_obj(c, where)
        out.append({
            "model": _expect_str(c, "model", where),
            "serial": _expect_str(c, "serial", where),
            "firmware": _opt_str(c, "firmware", where),
            "known_model": c.get("known_model"),
            "baseline": c.get("baseline"),
        })
    return out


def camera_key(cam: dict) -> tuple:
    """A camera's identity: (model, serial)."""
    return (cam["model"], cam["serial"])


def camera_label(cam: dict) -> str:
    return f"  {cam['model']}  ·  {cam['serial']}"


# ── Names ──────────────────────────────────────────────────────────────────────

def model_slug(model: str) -> str:
    """Filesystem-safe model slug: every space becomes an underscore.

    Must match the Rust side's firmware::model_slug() exactly — both are
    consumed by the same on-disk naming conventions (reference filenames,
    firmware archive directories).
    """
    return model.replace(" ", "_")


def is_plain_name_component(s) -> bool:
    """One plain file-name component: non-empty, not `.`/`..`, no `/`, `\\`
    or NUL. Mirrors ops::paths::is_safe_path_component (Rust); the test runs
    the same table as that function's `PLAIN_COMPONENT_TABLE`."""
    return (isinstance(s, str) and s not in ("", ".", "..")
            and not any(c in s for c in ("/", "\\", "\0")))


def is_id_fallback(serial: str) -> bool:
    """The `id-N` serial given to a body with no readable USB serial: `id-`
    then one or more ASCII digits (ops::target::is_id_fallback)."""
    rest = serial.removeprefix("id-") if serial.startswith("id-") else ""
    return rest != "" and all(c in "0123456789" for c in rest)


def reference_file_name(model, serial) -> "str | None":
    """File name of a camera's reference, `{model_slug}_{serial}.json`
    (ops::list::reference_filename), or None when the model or serial is not a
    plain name component (ops::paths::checked_reference_filename refuses it)."""
    if not (is_plain_name_component(model) and is_plain_name_component(serial)):
        return None
    return f"{model_slug(model)}_{serial}.json"


_ASCII_DIGITS = "0123456789"


def parse_fw_filename(name: str) -> tuple[str, str]:
    """Parse a Nikon firmware filename into (stem, version).

    'Z_9_0531.bin'  → ('Z_9',  '5.31')
    'Z6_3_0200.bin' → ('Z6_3', '2.00')
    'Z_30_0120.bin' → ('Z_30', '1.20')

    The version encoding is four ASCII decimal digits: first two are the major
    version, last two are the minor version (zero-padded). The stem is the
    model's *slug*, not the model: see suggest_fw_model.

    Returns (stem, '') for any filename that doesn't match the pattern.
    """
    stem = Path(name).stem
    parts = stem.rsplit('_', 1)
    if len(parts) == 2 and len(parts[1]) == 4 and all(c in _ASCII_DIGITS for c in parts[1]):
        return parts[0], f"{int(parts[1][:2])}.{parts[1][2:]}"
    return stem, ""


def suggest_fw_model(stem: str, known_models) -> str:
    """The model a firmware file name stem stands for.

    The stem is `model_slug(model)`, and the slug does not say whether an
    underscore was a space ("Z_9" is "Z 9") or part of the name ("Z6_3" is
    "Z6_3"), so it is not guessed: a model the GUI already knows (from the
    archive, the snapshots or a discovered camera) whose slug equals the stem
    is returned, else the stem unchanged for the user to correct. `firmware
    ls --model Z6_3` and the rollback model check match the stored model, so
    a wrong guess hides the entry.
    """
    for m in sorted(set(known_models)):
        if model_slug(m) == stem:
            return m
    return stem


def parse_port(text: str) -> int:
    """An FTP port as typed: ASCII digits only, 1..65535.

    Not Python's int(), which takes `2_1`, `+21` and non-ASCII digits; and
    port 0 is not a port anyone can listen on. Raises ValueError.
    """
    s = text.strip()
    if not s or not all(c in _ASCII_DIGITS for c in s):
        raise ValueError(f"{text!r} is not a number made of digits 0-9")
    n = int(s)
    if not 1 <= n <= 65535:
        raise ValueError(f"{n} is outside 1-65535")
    return n


# ── Zip entries (export/import) ────────────────────────────────────────────────

# The files of one firmware archive entry (src/firmware.rs): the binary, its
# metadata and the page it was retrieved from.
FIRMWARE_ENTRY_FILES = ("firmware.bin", "metadata.json", "source-page.html")


def accept_zip_entry(name: str) -> bool:
    """Return True if a zip archive entry should be imported.

    Accepted layouts:
      snapshots/<file>.json       — 2-part, .json only
      references/<file>.json      — 2-part, .json only
      firmware/<slug>/<ver>/<f>   — 4-part, <f> one of FIRMWARE_ENTRY_FILES

    Every part must be one plain component (no empty part, `.`, `..`, `\\`
    or NUL): a trailing `/`, a doubled `/` and a `.` part are rejected, not
    collapsed. The Rust GUI's `accept_zip_entry` takes the same cases from
    the same file, gui/tests-fixtures/zip_entry_names.json, which is the definition;
    export writes exactly what this accepts.
    """
    parts = name.split("/")
    if not all(is_plain_name_component(p) for p in parts):
        return False
    folder = parts[0]
    if folder in ("snapshots", "references"):
        return len(parts) == 2 and parts[1].endswith(".json")
    if folder == "firmware":
        return len(parts) == 4 and parts[3] in FIRMWARE_ENTRY_FILES
    return False


# ── Snapshot files ─────────────────────────────────────────────────────────────

_TRANSPORTS = ("usb", "ptpip", "file")


def _reject_json_constant(token):
    raise ValueError(f"{token} is not valid JSON")


def _is_u32(x) -> bool:
    return _is_int(x) and 0 <= x <= 0xFFFFFFFF


def snapshot_problem(doc) -> "tuple[str, str] | None":
    """None if *doc* (a parsed snapshot file) is a valid snapshot, else
    (error_kind, message): `json` for a malformed or incomplete file,
    `unsupported_version` for a `format_version` other than ours.

    The rule is `Snapshot::from_json`'s (src/snapshot.rs), the one the CLI's
    `ls` applies: format_version (u32), camera{model, serial, firmware},
    captured_at, transport, properties{name: {code (u32), value}} are all
    required (label may be null or absent; unknown keys are ignored), and the
    version is checked after the shape. Pinned by the fixture
    tests/fixtures/snapshot_validity/expected.json.
    """
    def bad(msg):
        return ("json", msg)

    if not isinstance(doc, dict):
        return bad("the top level is not an object")
    for key in ("format_version", "camera", "captured_at", "transport", "properties"):
        if key not in doc:
            return bad(f"missing field `{key}`")
    if not _is_u32(doc["format_version"]):
        return bad("`format_version` is not an unsigned 32-bit integer")
    cam = doc["camera"]
    if not isinstance(cam, dict):
        return bad("`camera` is not an object")
    for key in ("model", "serial", "firmware"):
        if not isinstance(cam.get(key), str):
            return bad(f"`camera.{key}` is missing or not a string")
    if not isinstance(doc["captured_at"], str):
        return bad("`captured_at` is not a string")
    if doc.get("label") is not None and not isinstance(doc["label"], str):
        return bad("`label` is not a string or null")
    if doc["transport"] not in _TRANSPORTS:
        return bad(f"`transport` is not one of {', '.join(_TRANSPORTS)}")
    props = doc["properties"]
    if not isinstance(props, dict):
        return bad("`properties` is not an object")
    for name, entry in props.items():
        if not isinstance(entry, dict):
            return bad(f"property {name!r} is not an object")
        if not _is_u32(entry.get("code")):
            return bad(f"property {name!r} has a missing or invalid `code`")
        if "value" not in entry:
            return bad(f"property {name!r} has no `value`")
    if doc["format_version"] != SNAPSHOT_FORMAT_VERSION:
        return ("unsupported_version",
                (f"unsupported format_version {doc['format_version']}; this build expects "
                 f"{SNAPSHOT_FORMAT_VERSION}"))
    return None


def snapshot_bytes_problem(raw: bytes) -> "tuple[str, str] | None":
    """snapshot_problem for a file's bytes: not UTF-8 is `io`, not JSON is `json`."""
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as e:
        return ("io", f"not valid UTF-8: {e}")
    try:
        doc = json.loads(text, parse_constant=_reject_json_constant)
    except (ValueError, RecursionError) as e:
        return ("json", f"not valid JSON: {e}")
    return snapshot_problem(doc)


def read_snapshot_file(path) -> "tuple[dict | None, tuple[str, str] | None]":
    """Read a snapshot file for display: (document, None) when it is valid
    by `snapshot_problem`'s rule, else (None, (error_kind, message)) --
    `io` for a file that cannot be read. Never raises."""
    try:
        raw = Path(path).read_bytes()
    except OSError as e:
        return None, ("io", str(e))
    problem = snapshot_bytes_problem(raw)
    if problem:
        return None, problem
    return json.loads(raw.decode("utf-8")), None


def firmware_meta_problem(meta_bytes: bytes, directory_slug: str, directory_version: str) -> "str | None":
    """Why a firmware `metadata.json` (with the directory it sits in) is not a
    valid archive entry, or None. Mirrors `FirmwareMeta` (src/firmware.rs):
    format_version 1, string model/firmware_version/bin_sha256, integer
    bin_size_bytes; and the directory must be `firmware/{slug(model)}/{version}`
    as `firmware add` writes it, or `archive_dir(model, version)` would not
    find the entry."""
    try:
        meta = json.loads(meta_bytes.decode("utf-8"), parse_constant=_reject_json_constant)
    except (UnicodeDecodeError, ValueError, RecursionError) as e:
        return f"metadata.json is not valid JSON: {e}"
    if not isinstance(meta, dict):
        return "metadata.json is not an object"
    if not _is_int(meta.get("format_version")):
        return "metadata.json has no integer format_version"
    if meta["format_version"] != FIRMWARE_META_FORMAT_VERSION:
        return (f"metadata.json format_version {meta['format_version']} is not "
                f"{FIRMWARE_META_FORMAT_VERSION}")
    for key in ("model", "firmware_version", "bin_sha256"):
        if not isinstance(meta.get(key), str) or not meta[key]:
            return f"metadata.json `{key}` is missing, empty or not a string"
    size = meta.get("bin_size_bytes")
    if not _is_int(size) or size < 0:
        return "metadata.json `bin_size_bytes` is missing or not a non-negative integer"
    if model_slug(meta["model"]) != directory_slug or meta["firmware_version"] != directory_version:
        return (f"metadata.json says {meta['model']} {meta['firmware_version']} but the directory is "
                f"{directory_slug}/{directory_version}")
    return None


# ── Property display (snapshot detail window) ─────────────────────────────────

_MISSING = "(missing)"
_MAX_SHOWN = 200


def _num(x) -> bool:
    """A JSON number (not a bool)."""
    return isinstance(x, (int, float)) and not isinstance(x, bool)


def _index_of(x) -> "int | None":
    """A usable non-negative integer index, else None (negative, bool, float
    and string indices have no position)."""
    return x if _is_int(x) and x >= 0 else None


def _compact(v) -> str:
    text = json.dumps(v, sort_keys=True, ensure_ascii=False, default=str)
    return text if len(text) <= _MAX_SHOWN else text[:_MAX_SHOWN] + "…"


def _range_scale(v: dict):
    """(lower, upper, steps) or None: the same test as value_shape::range_scale."""
    lower, upper, steps = v.get("lower"), v.get("upper"), v.get("steps")
    if _num(lower) and _num(upper) and _is_int(steps) and steps >= 0:
        return lower, upper, steps
    return None


def _fmt_range(v: dict) -> str:
    """A range's number. Same definitions as src/value_shape.rs: stepped needs
    steps >= 2 and a span, and its number is lower + index*(upper-lower)/(steps-1)
    from `value_index` (the SDK leaves `value` at 0.0 on these); an index at or
    past `steps` has no number; a continuous range is its `value`."""
    scale = _range_scale(v)
    if scale is not None:
        lower, upper, steps = scale
        if steps >= 2 and upper != lower:
            index = _index_of(v.get("value_index"))
            if index is None:
                return f"[stepped range: step index missing or invalid ({v.get('value_index')!r})]"
            if index >= steps:
                return f"[stepped range: step index {index} is past its {steps} steps]"
            return str(lower + index * (upper - lower) / (steps - 1))
    value = v.get("value")
    if not _num(value):
        return f"[range value {_MISSING if 'value' not in v else repr(value)}]"
    return str(value)


def _fmt_enum(v: dict) -> str:
    values = v.get("values", _MISSING)
    idx = v.get("value_index")
    if values == _MISSING:
        return f"[enum without a values list, index {idx!r}]"
    if isinstance(values, dict):
        # decode_enum_values (Rust) puts a JSON *object* ({"_unsupported_enum_type": N},
        # {"_invalid_physical_bytes": N}) where it could not decode the list.
        return f"[unrecognized enum data: {values}; index {idx!r}]"
    if not isinstance(values, list):
        return f"[enum values are not a list: {values!r}]"
    index = _index_of(idx)
    if index is None:
        return f"[enum index missing or invalid ({idx!r}) of {len(values)}]"
    if index >= len(values):
        return f"[index {index} / {len(values)}]"
    return fmt_cap_value(values[index])


def fmt_cap_value(v) -> str:
    """Render a snapshot property value as a compact, human-readable string.

    The shapes sdk::decode_value writes (tests/fixtures/capability_value_shapes.json):

    * enum  — `values` + `value_index`: the labelled/coded value at the index
              (packed-string enums arrive already decoded by Rust)
    * range — `lower`/`upper`/`steps`: see _fmt_range
    * `{"_type": "datetime"|"array"}`, `{"_unknown_type": N}` — shown as what they are
    * scalar — bool / int / float / str

    Anything else is shown as compact JSON; nothing here raises on malformed
    data, and a missing piece is shown as missing, never defaulted.
    """
    if v is None:
        return "null"
    if isinstance(v, str):
        return v
    if isinstance(v, bool):
        return "Yes" if v else "No"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, list):
        if len(v) <= 6:
            return "[" + ", ".join(fmt_cap_value(x) for x in v) + "]"
        return f"[{fmt_cap_value(v[0])}, … {len(v)} items]"
    if isinstance(v, dict):
        kind = v.get("_type")
        if kind in ("datetime", "array"):
            return f"({kind}: not captured)"
        if "_unknown_type" in v:
            return f"(unknown capability type {v['_unknown_type']!r})"
        if "lower" in v and "upper" in v:
            return _fmt_range(v)
        if "values" in v and "lower" not in v:
            return _fmt_enum(v)
        if "value_index" in v and "lower" not in v:
            return _fmt_enum(v)
        return _compact(v)
    return str(v)


_CAP_PREFIX = "kNkMAIDCapability_"


def property_rows(props) -> list:
    """(name, value, code) display rows for a snapshot's `properties`,
    sorted by name. A malformed entry or a missing code/value is shown as
    missing, not as `0x0000` or `None`."""
    if not isinstance(props, dict):
        return [("(properties)", f"[not an object: {type(props).__name__}]", _MISSING)]
    rows = []
    for name, entry in sorted(props.items(), key=lambda kv: str(kv[0])):
        display = str(name).removeprefix(_CAP_PREFIX)
        if not isinstance(entry, dict):
            rows.append((display, f"[malformed entry: {_compact(entry)}]", _MISSING))
            continue
        code = entry.get("code")
        code_text = f"{code:#06x}" if _is_u32(code) else _MISSING
        value_text = fmt_cap_value(entry["value"]) if "value" in entry else _MISSING
        rows.append((display, value_text, code_text))
    return rows


# ── Vendor ops ─────────────────────────────────────────────────────────────────

_PROPCODE_HEX_DIGITS = set("0123456789abcdefABCDEF")
_PROPCODE_DEC_DIGITS = set("0123456789")


def _propcode_strict_digits(body: str, allowed: set[str]) -> bool:
    """True if `body` is a non-empty run of ONLY `allowed` characters, with
    an optional leading '+' -- matching Rust's u16::from_str_radix/FromStr,
    which accept a leading '+' but reject internal whitespace and '_'
    digit-group separators. Python's own int() tolerates both of those
    (confirmed divergence: int(" 5", 16) and int("1_0") both succeed) --
    this exists so parse_propcode doesn't inherit that extra leniency and
    silently accept input the Rust CLI's parser would reject.
    """
    body = body.removeprefix("+")
    return len(body) > 0 and all(c in allowed for c in body)


def is_valid_u16(value: int) -> bool:
    """Whether `value` fits in a u16 (0-0xFFFF) -- the same range Rust's
    u16 type enforces."""
    return 0 <= value <= 0xFFFF


def parse_propcode(s: str) -> int:
    """Parse a vendor property code like the Rust CLI's parse_propcode:
    "0xD053"/"0XD053" (hex) or a plain decimal string.

    Raises ValueError if the string isn't a valid integer in either form,
    or is out of range for a u16 property code. Deliberately stricter than
    Python's own int() -- see _propcode_strict_digits -- since the Rust
    CLI's parser is the canonical/shared definition this must match exactly,
    not just "close enough" for the common cases.
    """
    text = s.strip()
    if text[:2] in ("0x", "0X"):
        hex_part = text[2:]
        if not _propcode_strict_digits(hex_part, _PROPCODE_HEX_DIGITS):
            raise ValueError(f"invalid hex property code {s!r}")
        value = int(hex_part, 16)
    else:
        if not _propcode_strict_digits(text, _PROPCODE_DEC_DIGITS):
            raise ValueError(f"invalid property code {s!r}")
        value = int(text, 10)
    if not is_valid_u16(value):
        raise ValueError(f"property code {s!r} out of range for a u16 (0-0xFFFF)")
    return value


# The FTP-profile form: (key, CLI flag, label). Only the flag names are the
# GUI's to know; which of them are required, how long each may be and what
# each is called in a refusal are the CLI's (`ptp_usb::FTP_PROFILE_FIELDS`),
# and the GUI shows the CLI's refusal instead of copying that table. The port
# is separate: it is not one of the six text fields.
FTP_TEXT_FIELDS = (
    ("profile_name", "--profile-name", "Profile name:"),
    ("ssid_24ghz",   "--ssid-24ghz",   "SSID (2.4GHz):"),
    ("ssid_5ghz",    "--ssid-5ghz",    "SSID (5GHz):"),
    ("host",         "--host",         "FTP host:"),
    ("username",     "--username",     "Username:"),
)
FTP_PASSWORD_KEY = "password"


def ftp_form_values(raw: dict) -> dict:
    """The FTP-profile form's values from the raw entry texts (keys of
    FTP_TEXT_FIELDS, `port` and FTP_PASSWORD_KEY). Text and password are sent
    exactly as typed: the CLI's `trimmed_for_required_check` is for its
    emptiness check only, and a value is never altered on the way to the
    camera. The port is parsed by parse_port (ValueError if it is not a port).
    Whether a field may be empty or too long is the CLI's to say."""
    values = {key: raw[key] for key, _flag, _label in FTP_TEXT_FIELDS}
    values["port"] = parse_port(raw["port"])
    values[FTP_PASSWORD_KEY] = raw[FTP_PASSWORD_KEY]
    return values


# ── Argv ───────────────────────────────────────────────────────────────────────
#
# Every value a user can type goes in as `--flag=value`: clap rejects a value
# that starts with `-` in the `--flag value` form (a profile named `-guest`),
# and accepts it in this one. A positional value goes after `--` for the same
# reason. These return the argv *after* the program name; global options come
# from global_argv().

def flag(name: str, value: str) -> str:
    return f"{name}={value}"


def global_argv(data_dir) -> list:
    return ["--no-usb-reset", flag("--data-dir", str(data_dir))]


def discover_argv() -> list:
    return ["discover", "--json"]


def ls_argv() -> list:
    return ["ls", "--json"]


def firmware_ls_argv() -> list:
    return ["firmware", "ls", "--json"]


def settings_show_argv() -> list:
    return ["settings", "show", "--json"]


def set_data_dir_argv(new_dir: "str | None") -> list:
    """`settings set-data-dir <abs-dir>`, or `--unset` for None/empty."""
    if not new_dir:
        return ["settings", "set-data-dir", "--unset"]
    return ["settings", "set-data-dir", "--", new_dir]


def snapshot_argv(serial: str, label: str) -> list:
    args = ["snapshot", flag("--serial", serial)]
    if label:
        args.append(flag("--label", label))
    return args


def ref_set_argv(data_dir, filename: str) -> list:
    """`ref set` for a snapshot file name from the listing. The CLI does the
    copy and the naming; a name that is not one plain component is refused
    here too, so it cannot be turned into another path."""
    if not is_plain_name_component(filename):
        raise ValueError(f"{filename!r} is not a plain snapshot file name")
    return ["ref", "set", "--", str(Path(data_dir) / "snapshots" / filename)]


def firmware_add_argv(src: str, model: str, version: str, force: bool = False) -> list:
    args = ["firmware", "add", flag("--model", model), flag("--version", version)]
    if force:
        args.append("--force")
    return args + ["--", src]


def firmware_browse_argv(model: str) -> list:
    return ["firmware", "browse", "--", model]


def vendor_read_argv(code: int, serial: "str | None") -> list:
    args = ["vendor-read", f"0x{code:04x}"]
    if serial:
        args.append(flag("--serial", serial))
    return args


def vendor_write_ftp_argv(values: dict, serial: "str | None", dry_run: bool) -> list:
    """`vendor-write-ftp` argv from the form values (keys of FTP_TEXT_FIELDS
    plus `port`). The password is NOT here: it goes on stdin (`--password-stdin`)
    so it is not in the process list."""
    args = ["vendor-write-ftp"]
    for key, flag_name, _label in FTP_TEXT_FIELDS:
        args.append(flag(flag_name, values[key]))
        if key == "host":
            args.append(flag("--port", str(values["port"])))
    args.append("--password-stdin")
    if serial:
        args.append(flag("--serial", serial))
    if dry_run:
        args.append("--dry-run")
    return args


# ── Results ────────────────────────────────────────────────────────────────────

def browse_outcome(returncode: int, stdout: str, stderr: str) -> tuple[bool, str]:
    """Interpret a finished `fleet firmware browse` run for display.

    Exit 0 means the download was captured and archived (or was already
    archived); the CLI's stdout says which. Anything else is a failure whose
    explanation is on stderr — and, when the embedded browser is unavailable,
    the manual-download steps are on stdout — so both are shown.
    """
    if returncode == 0:
        return True, stdout.strip()
    return False, "\n".join(p for p in (stderr.strip(), stdout.strip()) if p) or f"fleet exited {returncode}"


# For a real vendor-write-ftp, these are the only outcomes that mean the camera
# was not touched: 1 (a refusal or failure; src/main.rs says nothing was sent
# unless the message says so) and 2 (a usage error: nothing ran). Anything else
# (4, a signal, a crash, a timeout, a failure to start the wait) is not known to
# be harmless.
_WRITE_DEFINITELY_NOT_SENT = (1, 2)


def ftp_write_failure_text(returncode: "int | None", timed_out: bool, detail: str) -> str:
    """What to tell the user about a failed real `vendor-write-ftp`. Only exit
    1 and 2 are plain failures; every other outcome (exit 4, killed by a
    signal, an unexpected status, a timeout, no status at all) means the
    camera may have changed. *detail* is the CLI's own message, shown once."""
    if returncode in _WRITE_DEFINITELY_NOT_SENT and not timed_out:
        return detail
    return (f"{detail}\n\nThe camera may have changed: check the FTP profile in the camera's "
            "menu before trying again (it cannot be read back over USB).")


def join_output(stdout: str, stderr: str) -> str:
    """stdout then stderr, each stripped, for a box that shows everything the
    CLI said (warnings from a successful run are on stderr)."""
    return "\n".join(p for p in (stdout.strip(), stderr.strip()) if p)
