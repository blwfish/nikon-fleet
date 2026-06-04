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
