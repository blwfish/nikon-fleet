"""Pure helper functions — no tkinter, no subprocess. Importable for testing."""
from pathlib import Path


def strip_sdk_prefix(out: str) -> str:
    """Return the substring of *out* starting at the first '{'.

    The Nikon SDK binary unconditionally writes diagnostic lines (e.g.
    'InitializeSDK Execution duration: 209') to stdout before any JSON.
    This strips that prefix so json.loads() receives valid input.

    Raises ValueError if no '{' is present — callers should treat that as
    a subprocess failure, not a silent empty result.
    """
    return out[out.index('{'):]


def accept_zip_entry(name: str) -> bool:
    """Return True if a zip archive entry should be imported.

    Accepted layouts:
      snapshots/<file>.json       — 2-part, .json only
      references/<file>.json      — 2-part, .json only
      firmware/<slug>/<ver>/firmware.bin   — 4-part nested, .bin
      firmware/<slug>/<ver>/metadata.json  — 4-part nested, .json

    Rejects directory entries, path-traversal attempts, unknown folders,
    and wrong extensions.
    """
    parts = Path(name).parts
    if not parts:
        return False
    if any(p == ".." for p in parts):
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


def decode_packed_strings(values: list) -> list[str]:
    """Decode a Nikon SDK packed-string array into a list of option labels.

    The SDK stores enum option labels as individual characters with an empty
    string '' acting as a null-terminator between entries:

        ['J','P','E','G',' ','F','i','n','e','', 'R','A','W','', ...]
        → ['JPEG Fine', 'RAW', ...]

    Used for elem_type=7 enum capabilities in snapshot values.
    """
    options: list[str] = []
    current: list[str] = []
    for ch in values:
        if ch == "":
            if current:
                options.append("".join(current))
                current = []
        else:
            current.append(str(ch))
    if current:
        options.append("".join(current))
    return options


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

        if elem_type == 7 and idx is not None:
            options = decode_packed_strings(raw_vals)
            if 0 <= idx < len(options):
                return options[idx]
            return f"[index {idx} / {len(options)}]"

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
