# Property sweep: finding a setting nothing documents

`vendor-sweep` and `vendor-sweep-diff` (code: `src/prop_sweep.rs`). Built 2026-11-06; **not yet run on a body**.

## What it is for

The Nikon SDK's property list and the five properties the extended snapshot reads are not everything a body has.
Known gaps: the **AF activation** custom setting (AF-ON only vs shutter release + AF-ON; set the wrong way it can stop
NX Field activating the camera) and the **non-CPU lens data** menu (lens number, focal length, maximum aperture). Neither
is in the SDK's list or in any NX Field capture held here.

A body answers `GetDevicePropDesc` (`0x1014`) for each property it has, and `DevicePropNotSupported` (`0x200A`) for the
rest. Asking for every code in a block lists the block's properties with their data type, get/set flag, factory
default, current value and allowed values (a range or an enumeration). Two sweeps of one body with one menu setting
changed between them differ in exactly that setting's descriptor.

It sends nothing but `GetDevicePropDesc`. It cannot change the camera.

## Procedure (one sitting, body attached over USB, nothing else holding it)

Name each state with `--label`; the file name carries it.

```bash
nikon-fleet --data-dir "$(nikon-fleet settings data-dir)" vendor-sweep --label af-on-only   # state 1, the menu as it is
nikon-fleet --data-dir "$(nikon-fleet settings data-dir)" vendor-sweep --label af-on-only-again  # nothing changed: the noise
# change ONE setting in the camera's menu (AF activation: shutter release + AF-ON)
nikon-fleet --data-dir "$(nikon-fleet settings data-dir)" vendor-sweep --label shutter-and-af-on
nikon-fleet --data-dir "$(nikon-fleet settings data-dir)" vendor-sweep-diff <first> <third> --noise <second>
```

Files are in `<data-dir>/sweeps/` (not `snapshots/`: a snapshot listing must not meet a file that is not one). The
diff takes a file name there or a path. Properties that move by themselves between two sweeps with nothing changed (a
clock, a counter, a battery level) are listed under "set aside as noise" and kept out of the differences; `--keep-noise`
puts them back.

Per target:

| target | how |
|---|---|
| AF activation | state 1 / state 2 as above. The diff should show one property, its current value changing between the two values, and its allowed values |
| non-CPU lens data | change one of its fields at a time (lens number, then focal length, then maximum aperture): three sweeps, three diffs. Setting a lens number first may be needed for the others to appear |
| FTP profile option bits | **not a device property**: they are in the profile blob. Change one profile setting on the camera, run `vendor-read-ftp` (or `snapshot`, which captures the profile) before and after, and compare "Option flags". Only the SFTP bit (0x0008) is confirmed |

A property that was found goes into `EXTENDED_PROPS` (`src/extended.rs`) so snapshots, `diff`, `check` and the
owner-data restore cover it.

## Output

`vendor-sweep` prints the counts (asked / present / not supported / refused / no usable answer) and the file path.
The default blocks are `0x5000-0x50FF` and `0xD000-0xD4FF` (about 1 800 codes); `--range LO-HI` (repeatable, `0x` hex
or decimal, inclusive, merged when overlapping or touching) names others, for example `--range 0xD000-0xDFFF` if a
setting is not found in the default vendor block.

Each code asked is recorded as exactly one of: *present* (the descriptor bytes as received, whether DeviceInfo lists
the code, and the decoded fields), *refused* (a response code other than `0x200A`), *no usable answer* (the exchange
failed), or *not supported*, which is the absence of an entry and is counted. The decoded fields are the code, data
type, get/set flag, default, current value, the range or enumeration and any bytes after them. A field the parser
cannot read ends the parse there; what came before it is kept and `fault` says why. The raw bytes are always kept.

## When it stops

A timeout, a poisoned session or a USB error ends the sweep at once (a timeout can leave the pipe stalled and every
later answer would be untrustworthy); three unusable replies in a row end it too. What was found is saved, the file
records where it stopped (`aborted`), `vendor-sweep` exits non-zero, and `vendor-sweep-diff` compares only the codes
both dumps cover and says so. Run it again; if it stops in the same place, that code is the finding.

## Limits

- It finds device properties with a descriptor. A setting reachable only through a vendor operation (`0x943B` and the
  like) or only through MAID will not appear.
- A setting that is not in the swept blocks needs a wider `--range`.
- A descriptor's get/set flag and allowed values are state-dependent (a setting can be read-only in one shooting mode),
  so sweep with the camera in one mode throughout.
- What a value *means* is not in the descriptor: the diff shows 0 becoming 1, and the menu says which is which.
