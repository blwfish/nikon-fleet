"""Pure helper functions — no tkinter, no subprocess. Importable for testing."""
from pathlib import Path


def strip_sdk_prefix(out: str) -> str:
    """Return the substring of *out* starting at the first line-starting '{'.

    The Nikon SDK binary unconditionally writes diagnostic lines (e.g.
    'InitializeSDK Execution duration: 209') to stdout before any JSON.
    This strips that prefix so json.loads() receives valid input.

    We search for a '{' that is either at position 0 or immediately after a
    newline, so a brace embedded inside a diagnostic message (e.g. a timing
    report with '{elapsed: 42ms}') is not mistaken for the JSON start.

    Raises ValueError if no line-starting '{' is present — callers should
    treat that as a subprocess failure, not a silent empty result.
    """
    if out.startswith('{'):
        return out
    idx = out.find('\n{')
    if idx == -1:
        raise ValueError("no JSON object found in SDK output")
    return out[idx + 1:]


def model_slug(model: str) -> str:
    """Filesystem-safe model slug: spaces become underscores.

    Must match the Rust side's firmware::model_slug() exactly — both are
    consumed by the same on-disk naming conventions (reference filenames,
    firmware archive directories).
    """
    return model.replace(" ", "_")


def accept_zip_entry(name: str) -> bool:
    """Return True if a zip archive entry should be imported.

    Accepted layouts:
      snapshots/<file>.json       — 2-part, .json only
      references/<file>.json      — 2-part, .json only
      firmware/<slug>/<ver>/firmware.bin   — 4-part nested, .bin
      firmware/<slug>/<ver>/metadata.json  — 4-part nested, .json

    Rejects directory entries, path-traversal attempts, unknown folders,
    and wrong extensions.

    Splits on "/" directly rather than using Path(name).parts: pathlib
    silently drops "." (current-dir) components, which let
    "snapshots/./foo.json" diverge from gui/src/main.rs's accept_zip_entry
    (Rust's Path::components() does NOT drop CurDir, and rejects it) even
    though both sides are commented "must stay in sync." Manual splitting
    treats "." as just another path segment, matching Rust's stricter
    behavior — this repo's zip entries always use "/" as the separator
    regardless of platform, so this isn't an OS-portability regression.
    """
    parts = name.split("/")
    if any(p in ("", ".", "..") for p in parts):
        return False
    folder = parts[0]
    if folder not in ("snapshots", "references", "firmware"):
        return False

    if folder in ("snapshots", "references"):
        return len(parts) == 2 and name.endswith(".json")

    # firmware — nested layout: firmware/{model_slug}/{version}/{file}
    if len(parts) != 4:
        return False
    filename = parts[3]
    return filename == "firmware.bin" or filename == "metadata.json"


def fmt_cap_value(v) -> str:
    """Render a snapshot property value as a compact, human-readable string.

    Handles the four shapes the Nikon SDK writes into snapshots:

    * elem_type 7  — packed-string enum: decode chars → labels, pick by value_index
    * elem_type 2  — integer-code enum: return values[value_index] as a string
                     (camera-menu labels for these require MaidLayer resource strings,
                     which will be added in the editing increment)
    * range dict   — float range: return the 'value' field
    * scalar       — bool / int / float / str: return directly
    """
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
        elem_type = v.get("elem_type")
        idx       = v.get("value_index")
        raw_vals  = v.get("values", [])
        # decode_enum_values (Rust) falls back to a JSON *object* —
        # {"_unsupported_enum_type": N} or {"_invalid_physical_bytes": N} —
        # for an array-element type it doesn't decode. Guard for that shape
        # explicitly rather than assuming "values" is always a list: a bare
        # `raw_vals[idx]` on a dict with idx=0 raises KeyError (the only key
        # is a string, not the int 0), not an IndexError, and used to crash
        # the snapshot-detail window instead of rendering gracefully.
        if not isinstance(raw_vals, list):
            return f"[unrecognized enum data: {raw_vals}]"

        if elem_type == 7 and idx is not None:
            # raw_vals is already decoded by Rust (decode_enum_values) into
            # ["JPEG Fine", "RAW", ...] — this file has no char-by-char
            # packed-string decoder of its own; that logic lives once, in
            # Rust, not duplicated here.
            if 0 <= idx < len(raw_vals):
                return str(raw_vals[idx])
            return f"[index {idx} / {len(raw_vals)}]"

        if elem_type is not None and idx is not None:
            # Integer-code enum (elem_type 2, etc.) — raw code until resource
            # strings are parsed from MaidLayer.config in the editing increment.
            if 0 <= idx < len(raw_vals):
                return str(raw_vals[idx])
            return f"[index {idx}]"

        if "lower" in v and "upper" in v:
            # Float range capability
            return str(v.get("value", "?"))

        return str(v.get("value", v))
    return str(v)


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
    if body.startswith("+"):
        body = body[1:]
    return len(body) > 0 and all(c in allowed for c in body)


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
    if not (0 <= value <= 0xFFFF):
        raise ValueError(f"property code {s!r} out of range for a u16 (0-0xFFFF)")
    return value


def parse_fw_filename(name: str) -> tuple[str, str]:
    """Parse a Nikon firmware filename into (model, version).

    'Z_9_0531.bin'  → ('Z_9',  '5.31')
    'Z6_3_0200.bin' → ('Z6_3', '2.00')
    'Z_30_0120.bin' → ('Z_30', '1.20')

    The version encoding is four decimal digits: first two are the major
    version, last two are the minor version (zero-padded).

    Returns (stem, '') for any filename that doesn't match the pattern.
    """
    stem = Path(name).stem
    parts = stem.rsplit('_', 1)
    if len(parts) == 2 and len(parts[1]) == 4 and parts[1].isdigit():
        return parts[0], f"{int(parts[1][:2])}.{parts[1][2:]}"
    return stem, ""
