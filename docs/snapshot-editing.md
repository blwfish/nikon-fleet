# Editing a copy of a snapshot

Code: `src/snapshot_edit.rs` (the rules, on a value), `src/ops/edit_snapshot.rs` (the files), CLI `nikon-fleet edit …`.
Built 2026-11-06 (step 1 of the snapshot editor: library and CLI; step 2: the GUI, below; the FTP profile field editor and
the FTP test are later steps). Needs no camera.

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

Not editable: strings, dates, arrays and other shapes; properties the restore never copies (`code_policy`). The fields of an FTP
profile, adding one and duplicating one are in `docs/ftp-profiles.md`. Reordering is by rebuilding the list on the
camera, which restoring a reordered list already does.

What only the camera can answer is answered when a restore is planned, not here: whether a property is writable now,
whether this firmware takes the value, the length limits of IPTC fields, the active FTP profile.

No password or key is written to the edit log; a profile is named by its stored name.

## Concurrency

An in-place edit reads the file, changes it, and replaces it whole (`write_atomic`). Two editors working on one file at
once (two windows, or the GUI and the CLI) would have the second overwrite the first's change; there is no lock.

## In the GUI (step 2)

`gui/src/snapshot_editor.rs`, in the snapshot viewer; no new window.

- **Edit a copy…** on each row of a body's snapshot list (any row: a captured snapshot, a reference, an edited copy).
  It makes the copy in `snapshots/` (a local file operation, not a worker command) and opens it. The original is only
  read. Rows that are edited copies say `✎ edited`, and have no **Set ref** button.
- An edited copy's window has a strip at the top (where it came from, a **History** of every change, and the last
  change's result or refusal) and controls in place of the values: a choice is a drop-down (the SDK's names where
  loaded, with the number), a position on a scale a slider (committed when the drag ends or on a click, not on every
  frame), a number a text box, a switch a check box. A property that cannot be edited keeps its value and says why on
  hover. Properties changed in this copy are marked `✎`.
- **Owner data (edit)** above the table: Artist, Copyright and Image comment (with a character count against the
  limit), the two switches, each IPTC preset's name and fields, and the FTP profile list with Up, Down and Delete (a
  delete needs a second press on the same row). A text box commits when it loses focus (Enter, Tab, a click elsewhere)
  and Escape gives up.
- Every change is saved at once through the library's edit functions, which do all the checking; the window then
  reads the file again, so what it shows is always what is stored. A refusal shows its reason and the field goes back
  to the stored value. While the file is being read again the controls wait.
- A snapshot read from a camera opens exactly as before, read-only.

What the tests cover: the app path end to end (copy, open, change, reload, refusal, a capture refused), the controls
driven with real input events (typing and Enter, Escape, a click on a switch, a click on a slider), the two-press
delete and the ends of the list, and that the library offers a control only where its setter accepts the value.
**Not covered:** the drop-down's item click, dragging a slider, and how the window looks (it has not been seen on a
screen).
