# Editing a copy of a snapshot

Code: `src/snapshot_edit.rs` (the rules, on a value), `src/ops/edit_snapshot.rs` (the files), CLI `nikon-fleet edit …`.
Built 2026-11-06 (step 1 of the snapshot editor: library and CLI; step 2: the GUI, below; the FTP profile field editor
(`src/ftp_edit.rs`, `docs/ftp-profiles.md`) and the FTP test came after). Needs no camera. Nothing here has been run
against a camera: what a camera does with an edited copy that is restored to it is **unverified**.

## The rule

A snapshot read from a camera is a record and is **never modified**, and a reference even less so. To change a setting in
what will later be restored to a body, make an **edited copy**. The copy carries `derived_from` in its file: the file it
was copied from, when the source was captured, when the copy was made and last edited, and every change made in it
(oldest first, carried over when an edited copy is itself copied). That field is what makes a file editable and what marks
it "edited" in `ls` (`[edited copy]`) and in the listing JSON (`"edited": true`, absent for a captured snapshot). Every
edit function refuses a snapshot without it, so a caller that forgot to check still cannot change a captured one.

That guarantee does not rest on each editor remembering a first line. The three ways the editors reach a snapshot's
contents for writing (a property, the extended section, the FTP list) each refuse a snapshot that is not a copy, and a
test lists the public editors from the source text and fails for one it does not run against a capture. What it does
not defend against: a person adding `derived_from` to a captured file by hand (the marker is a field in the file; the
rule guards against mistakes, not against editing your own data directory).

- The copy keeps the source's `captured_at` (the values were read then). Its file name is built from `created_at`, so
  two copies of one snapshot do not share a name; listings order by `captured_at`, so a copy sorts beside its source
  (it ties with it on that time).
- Edited copies stay editable in place; captured snapshots and references do not.
- An edited copy **cannot become a reference** (`ref set` refuses it): a reference is what `check` compares a body to.
- A copy's label may not start `mcp-` or be `restore-ext` (the MCP server and `restore-extended` count their captures and restore points by those labels: `ops::retention::kind_of_label`).
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
| the FTP profile list | `edit ftp-delete FILE INDEX`, `edit ftp-move FILE FROM TO` | index exists; the list was read whole; slots and count renumbered; the active slot never names a slot past the end |
| (history) | `edit log FILE` | |

Not editable: strings, dates, arrays and other shapes; properties the restore never copies (`code_policy`). The fields of an FTP
profile, adding one and duplicating one are in `docs/ftp-profiles.md`. Reordering is by rebuilding the list on the
camera, which restoring a reordered list already does.

What only the camera can answer is answered when a restore is planned, not here: whether a property is writable now,
whether this firmware takes the value, the length limits of IPTC fields, the active FTP profile.

No password or key is written to the edit log; a profile is named by its stored name.

**The active FTP profile** is the slot number the camera reported when the snapshot was read. Delete and move keep that
number as it is (it stays in range; an emptied list has none), not the profile it pointed at. That is deliberate: a
restore cannot set the active profile, and what the camera reports is not a setting (its slot has stayed 0 in the
replies seen), so following a profile around would invent a value; the edited copy may show a different "active
profile" than the capture, and that difference is not something a restore will change.

**Required fields and the template (FTP profiles).** `vendor-write-ftp` refuses a profile missing any of six fields
(`ptp_usb::ftp_profile_missing_fields`). The editor does not, and a restore does not check it either: cameras make
profiles without a login (two real blobs in the test fixtures), so a rule that refused them would refuse profiles a
camera made. When an edit leaves a profile lacking something `vendor-write-ftp` would require, the result says so.
A new profile starts from the editor's template (`ftp_edit::template`). **It used to** carry the capture rig's camera
address (192.168.66.87/24, the owner's Z 9 on the rig), so every profile added here, and every restore that wrote one,
carried another person's camera address. **It now has no stored camera address** (0.0.0.0/0) and the 2.4 GHz tag kind
byte `0x30`, which is the form of the one camera-made profile in `tests/fixtures/ftp_profile_vectors.json` that has none
(`hotspot_no_address`; the other five fixtures have an address, 192.168.66.87/24, 192.168.68.83/22 or 10.0.0.251/24, and
kind `0x01`). Option bits (`0x0005`), ports (21 and 8080) and the 5 GHz tag kind (4) keep their documented defaults, so the
combination is the nearest the fixtures support rather than a copy of one: the strict decoder takes it (it accepts any
value in those tag bytes). A **duplicate** is unchanged: it copies its source whole, that profile's own camera address
included. The result of an add says the profile has no stored camera address and that this is unverified. `vendor-write-ftp`
is unchanged too: it builds a profile with the encoder and still writes the captured form. **Not verified on a camera**:
whether a body lists, keeps and connects with a profile that has no stored address, or with one that has no login or no
Wi-Fi name. To check on a body: add a profile in the editor, restore it with `restore-extended`, then read the body back
(`vendor-read-ftp`) and see whether the camera lists the profile and can connect with it; if it does not, the next step is to
start from a copy of one of that body's own profiles.

## Concurrency

An in-place edit reads the file, changes it, and replaces it whole (`write_atomic`). Two editors working on one file at
once (two windows, or the GUI and the CLI) would have the second overwrite the first's change; there is no lock and no
check that the file is still the one that was read (a known gap, in `src/ops/edit_snapshot.rs`, not fixed).

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
