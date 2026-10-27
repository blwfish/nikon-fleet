# reference_parity fixtures

One data directory, one expected answer, three implementations that must agree
on which snapshots are "the reference" (a snapshot is the reference iff its
file is byte-identical to `references/{model_slug}_{serial}.json`, with model
and serial taken from the snapshot itself):

- `nikon_fleet::ops::list::list_dir` (the CLI and, later, the MCP),
- the Rust GUI's `list_snapshots` (`gui/src/main.rs`),
- the Python GUI's `snapshot_rows` (`gui/fleet_lib.py`).

`expected.json` maps a serial to `{snapshot file: is_reference}` for every
snapshot that serial owns. `broken.json` is not a snapshot: the GUIs skip it
(with a warning); `ops::list` lists it as unreadable. Cases: a promoted snapshot
and its byte-identical twin (both true); same timestamp but different content;
same data re-serialised; differing only by a trailing newline; no reference at
all; `id-1` shared by two models with a reference for only one; a non-ASCII
label; a serial containing `../`; and a serial `x/y` whose naive reference path
(`references/Z_9_x/y.json`) exists and is byte-identical, which must not count.
