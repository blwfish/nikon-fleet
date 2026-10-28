# Snapshot validity fixture

One directory of snapshot files, each either valid or wrong in one specific way, and `expected.json`:
file name -> `"ok"` (listed with its fields) or the `error_kind` an unreadable entry must carry
(`SnapshotError::tag`: `json` for a malformed or incomplete file, `unsupported_version` for a
`format_version` other than 1).

The rule is `Snapshot::from_json`'s, and every reader of snapshot files must agree with it:
`nikon_fleet::ops::list` (and so `nikon-fleet ls --json`, which the test in
`tests/list_json_cli.rs` runs over this directory), `SnapshotSummary`, the egui GUI's listing and the
Python GUI's `snapshot_rows`. `format_version`, `properties` and `transport` are all required.
