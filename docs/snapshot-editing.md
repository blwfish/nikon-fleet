# Editing a copy of a snapshot

Code: `src/snapshot_edit.rs` (the rules, on a value), `src/ops/edit_snapshot.rs` (the files), CLI `nikon-fleet edit …`.
Built 2026-11-06 (step 1 of the snapshot editor: library and CLI; the GUI window, the FTP profile field editor and the
FTP test are later steps). Needs no camera.

## The rule

A snapshot read from a camera is a record and is **never modified**, and a reference even less so. To change a setting in
what will later be restored to a body, make an **edited copy**. The copy carries `derived_from` in its file: the file it
was copied from, when the source was captured, when the copy was made and last edited, and every change made in it
(oldest first, carried over when an edited copy is itself copied). That field is what makes a file editable and what marks
it "edited" in `ls` (`[edited copy]`) and in the listing JSON (`"edited": true`, absent for a captured snapshot). Every
edit function refuses a snapshot without it, so a caller that forgot to check still cannot change a captured one.

- The copy keeps the source's `captured_at` (the values were read then) and is named, and ordered, by `created_at`.
- Edited copies stay editable in place; captured snapshots and references do not.
- An edited copy **cannot become a reference** (`ref set` refuses it): a reference is what `check` compares a body to.
- A copy's label may not start `mcp-` (the MCP server counts its own captures and restore points by that prefix).
- A reference can be copied; it is only read.
- Nothing about an edited copy is special to restore or transplant: it is a snapshot, so a restore plan made from one
  writes the edited values (pinned by a test that plans from a fake body), and the plan, the confirmation, the restore
  point and the read-back all apply.

## What can be edited, and what is checked at edit time

| item | command | checked here |
|---|---|---|
| a choice (an enumeration) | `edit set FILE NAME VALUE` | VALUE is one of the values the snapshot lists for it (by value, not position) |
| a position on a scale | same | on the scale; a stepped range takes the nearest grid point if within a twentieth of a step; a continuous range within its ends |
| a number / a switch | same | a whole number stays whole; a switch is true/false/1/0 |
| Artist, Copyright, Image comment | `edit set-string FILE ITEM TEXT` | Latin-1, no control characters (the write's own check), at most 36 / 54 / 36 characters (measured on Z 9, Z6III, Z 5) |
| EnableCopyright, IPTC preset select | `edit set-switch FILE ITEM VALUE` | 0..=1, 0..=13 (the write's own limits) |
| an IPTC preset's name or a field | `edit set-iptc FILE SLOT FIELD TEXT` | slot exists and is a preset; no NUL |
| the FTP profile list | `edit ftp-delete FILE INDEX`, `edit ftp-move FILE FROM TO` | index exists; the list was read whole; slots and count renumbered; the active slot always names a profile |
| (history) | `edit log FILE` | |

Not editable: strings, dates, arrays and other shapes; properties the restore never copies (`code_policy`). Not yet:
changing the **fields** of an FTP profile, adding one, duplicating one (they need a writer that builds a profile from
fields; the existing encoder fixes the server port, folder and protocol). Reordering is by rebuilding the list on the
camera, which restoring a reordered list already does.

What only the camera can answer is answered when a restore is planned, not here: whether a property is writable now,
whether this firmware takes the value, the length limits of IPTC fields, the active FTP profile.

No password or key is written to the edit log; a profile is named by its stored name.

## Concurrency

An in-place edit reads the file, changes it, and replaces it whole (`write_atomic`). Two editors working on one file at
once (two windows, or the GUI and the CLI) would have the second overwrite the first's change; there is no lock.
