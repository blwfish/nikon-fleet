# NX Field FTP-profile session, and a PTP/IP profile read — 2026-07-10/11

What a live NX Field → camera session over the Raspberry Pi access point showed about Nikon FTP
profiles, plus a from-scratch PTP/IP client (`scripts/ptpip_read_ftp.py`) that reads them without USB.
Follows `docs/usb-vs-wifi-camera-behavior-handoff-2026-11-05.md` (which found the USB read op
`0x90ED` and the blob layout) and `docs/nx-field-session-2026-07-09.md`.

Everything here was observed on one camera, the Z9 `Z_9_3023668` (its serial was read over USB later in the session; the owner called it "Z9a"): its slot 1
still holds `txid-edit` (host `192.0.2.2`, user `txid2`), the throwaway profile written to Z9a
earlier. The camera model was not re-read in this session. Treat anything model-specific as unverified.

## Setup

- Pi access point `nikon-snoop`, `192.168.66.0/24`, Pi at `192.168.66.1`. Camera on a **manual** IP
  (`192.168.66.40`), which is why it has no DHCP lease. iPhone running NX Field at `192.168.66.37`.
- The Pi ran `mini_ftpd.py` (plain FTP, port 21, login `fleet` / `12345678`, root `/home/pi/ftproot`),
  its normal `sshd` (port 22), and `tcpdump` on `wlan0`.
- Capture: `captures/2026-07-10/session-20260710-234934-nxfield-fail.pcap` (178 MB, gitignored, so only
  on the MacBook and the Pi; sha256 `b9c95054a4e5131bf283a75b2d5f426e3e72b4b4d1702ad5e7d195d27b35ec46`).
  Timestamps below are seconds from the start of that capture unless a clock time is given. The Pi's
  clock is wrong (it says July 2026), so clock times are Pi-local and only good for ordering.

## 1. "NX Field turns off with no message" = the camera refuses the TCP connection

The first symptom. In the capture, the phone opens TCP connections to the camera's port 15740
(PTP/IP) and gets an immediate reset (`RST`) each time, six attempts over about a minute. Probing the
camera from the Pi showed 15740, 21, 22, 80, 443, 8080 all closed, while it still answered pings. Once
the camera's connect-to-smart-device state was switched on, port 15740 opened and the same phone
connected on its next try. So the camera was up on Wi-Fi but not listening for PTP/IP. NX Field showed
no message. The camera model has to be in that state before any PTP/IP client can work.

## 2. NX Field's FTP profile edit is a rewrite of slot 0

NX Field's profile editor does `0x90E8`, `0x90EB`, `0x90ED(0)` (read slot 0), and on save
`0x90EE(0, 0)` (write slot 0). In this session it never touched slot 1 and never added a profile: the
`0x90EB` count stayed at **2** after every NX Field write. NX Field only ever sends the second
parameter as `0`. **Adding a profile does work, with the second parameter set to `1`** (found later in
the session with our own client, section 5).

History of slot 0, decoded with the blob layout from the handoff doc (password lengths only):

| Capture time | Event | Name | Host | Type / port | User, pw len |
|---|---|---|---|---|---|
| 165 s | NX Field reads | `SNOOP` | `192.168.66.1` | FTP / 21 | `fleet`, 8 |
| 176 s | **NX Field writes** | `NewFTPprofile` | `193.168.68.1` | SFTP / 22, folder `~` | `fleet`, 9 |
| 1237 s | read, after fixes made at the camera | `NewFTPprofile` | `192.168.66.1` | FTP / 21 | `fleet`, 8 |
| 1358 s | **NX Field writes** | `Snoop2` | `192.168.66.1` | SFTP / 22 | `fleet`, 8 |
| 1539–1703 s | reads, after switching back at the camera | `Snoop2` | `192.168.66.1` | FTP / 21 | `fleet`, 8 |
| 1794 s | **NX Field writes** | `NxProfile` | `192.168.66.1` | SFTP / 22 | `fleet`, 8 |
| 2077 s | read, after setting user/password at the camera | `NxProfile` | `192.168.66.1` | SFTP / 22 | `pi`, 7 |

- Between writes the owner fixed values in the camera's own menu, and the camera kept them (the reads
  show FTP / 21 again). Each of the three NX Field writes therefore overwrote slot 0, replacing the
  working `SNOOP` profile.
- The write's SSID, camera IP, username and the unexplained "second port" (8080) matched what had
  been read, and so did host, folder and password in the 2nd and 3rd writes. That suggests NX Field
  prefills its form from the `0x90ED(0)` read, but the form is not visible in the capture, so that is
  an inference. **Whether NX Field defaults to SFTP or the owner picked it is not established**; the
  owner says they chose SFTP.
- The first write carried four mistakes (host typo `193.168.68.1`, SFTP/22, folder `~`, a 9-character
  password). The camera sent exactly what was in the profile.

### What each mistake looked like (camera error numbers reported by the owner)

The error numbers are the camera's own, read off its screen; the causes are matched to the Pi's FTP log
and the capture, not to a Nikon document.

- Wrong host: no number. The camera ARPs repeatedly for the non-existent host (`193.168.68.1`, later
  `192.168.68.1`).
- **Error 31**: the camera was set to SFTP and sent an SSH banner (`SSH-2.0-libssh2_1.9.0`) to the
  plain-FTP server on port 21, which answered `502 not implemented`. (31 was reported before this
  was inspected; this is the matching failure seen at that moment.)
- **Error 32**: login rejected. Plain FTP: camera sent `PASS 123456789` where the server wanted
  `12345678` (`530 Login incorrect`). Later, on SFTP: `sshd` logged `Failed password for invalid user
  fleet`, because the Pi had no `fleet` account.
- **Error 34**: login worked, then the camera sent `CWD ~` and the server answered
  `550 No such directory`. Clearing the profile's folder removed the `CWD`.
- With a valid profile the camera's connection test is `USER`, `PASS`, `PWD`, `PASV`, `TYPE A`,
  `NLST`, `QUIT`.

### Every profile save makes the camera restart its Wi-Fi

After each profile change the camera re-associated with the access point (station "connected time"
reset, gratuitous ARP probe from `0.0.0.0`, fresh mDNS announcements) and the open PTP/IP sessions
died. The camera did this roughly every minute while its profile pointed at an unreachable host, and
stopped when the host was right. Whether the restart is caused by the save itself or by the failing
host is not separated by this data.

## 3. Auto-send behaviour (JPEG only, with RAW+JPEG shooting)

- With auto-send on and "send only JPEG" selected (owner's settings; not visible in PTP), two shots
  produced exactly two FTP uploads: `_BLW9412.JPG` (979,513 B) and `_BLW9413.JPG` (980,443 B). A later
  SFTP-profile shot produced `_BLW9414.JPG` (1,024,669 B), into the login's home directory.
- Before each upload the camera sent `SIZE` for the `.JPG`, `.NEF`, `.HIF` and `.WAV` names, then a
  `STOR` for the `.JPG` only. This is the same collision-avoidance probe described in
  `docs/nx-field-session-2026-07-09.md`.
- The camera's PTP events report storage ids `0x10001` and `0x20001` (the two card slots) and, at the
  time of the first photo, two `ObjectAdded` events (handles `0x091b24c2`/`…c3`). Whether those are
  one file per slot or something else was **not determined**. An earlier guess that they were RAW+JPEG
  was withdrawn when the camera showed JPEG Basic/S.

## 4. A PTP/IP client written from the capture: `scripts/ptpip_read_ftp.py`

Reads FTP profiles over Wi-Fi with plain TCP, no NX Field, no USB. By default it is read-only; an
experimental `--write` mode (section 5) can replace or add a profile and is a dry run unless `--send`
is given.

```bash
python3 scripts/ptpip_read_ftp.py 192.168.66.40            # slot 0
python3 scripts/ptpip_read_ftp.py 192.168.66.40 --slot 1   # any slot below the count
```

Python 3 standard library only. It needs a machine on the camera's network (the Pi, or a MacBook on the
same Wi-Fi). The password and the "5 GHz" field print as `*`; `--out FILE` saves the raw blob **with**
the password in clear, so keep it private.

### What it sends, and what the camera accepted

- Command channel to port 15740: `InitCommandRequest` (GUID, name, protocol 1.0), then an event
  channel `InitEventRequest` with the connection number the camera returns.
- `OpenSession(1)`, then `0x90E8` with data-out `01 00 00`, `0x90EB`, `0x90ED(slot)`. All returned
  `0x2001`. **None of NX Field's other start-up queries were needed.** `--prefix` replays some of them.
- NX Field uses transaction id **1 on every request**; the client does the same (`--incr-tid` exists
  but was not needed).
- Slot 0 and slot 1 both read. Slot 1 was the `txid-edit` test profile, which NX Field never shows.
  `0x90EB` reported 2 profiles, limit 20.

### The identity check

- The client GUID NX Field sent in every session was `01004e58000000000000ff900ae772a7`, device name `@@`.
  The July captures of sessions to `192.168.66.87` (from `192.168.66.97`) used
  `01004e58000000000000e27b7d25b9a2`, same name; whether that was another phone is not known.
  Pattern: `01 00 4e 58`, six zero bytes, six varying bytes.
- A GUID with that prefix and an invented tail (`…c0ffee123456`) was **accepted**. A GUID without the
  prefix (`deadbeef…`) was **refused** with an `InitFail` packet, reason code `1`. Changing the device
  name made no difference. So the camera checks the GUID's shape, not a pairing list. No pairing or
  authentication step appeared in the handshake.
- **One session at a time (probable, not verified):** the camera's connection number rose by one with
  each new connection, and the owner saw NX Field drop when the script first ran. A program on a
  MacBook would therefore disconnect the phone while it reads.

## 5. Writing and adding profiles over Wi-Fi (experimental)

`scripts/ptpip_read_ftp.py --write FILE` sends `0x90E8` (setup) then `0x90EE(slot, param2)` with the
raw blob. Dry run unless `--send`. It refuses a slot at or past the stored count unless
`--allow-create`. Camera: the same one as above, two stored profiles at the start. Raw blobs come from
`--out`.

| Call | Reply | Effect (read back over a fresh connection) |
|---|---|---|
| `0x90EE(1, 0)` with the profile already in slot 1 | `0x2001` | that profile is now slot 0; the old slot 0 is slot 1 |
| `0x90EE(1, 0)` with a new-named blob | `0x2001` | slot 1's old profile is **replaced**, and the new one moved to slot 0; count unchanged |
| `0x90EE(2, 0)` (one past the end) | `0x201F` TransactionCancelled | nothing changed, camera did not restart |
| **`0x90EE(2, 1)`** (slot = count, second parameter 1) | `0x2001` | **a profile is added**: count 2 → 3, new profile at slot 0, the others shifted down |
| `0x90EE(2, 2)`, `0x90EE(2, 0xFFFFFFFF)`, `0x90EE(0xFF, 0)` | `0x201F` | nothing changed |
| `0x90EE(0xFFFFFFFF, 0)` | **no reply (timeout)** | **the camera's network stack hung** (see below) |

- **Rule so far:** writing a profile into slot N with second parameter 0 replaces slot N and moves it to
  the front. With the second parameter 1 and slot = count it inserts a new profile at the front.
  What the second parameter means in general is unknown; only 0 and 1 have worked.
- **List order is not the selection.** Right after the adds, the camera kept logging in with a profile that
  was *not* in slot 0, and the `0x90EB` "slot" field read `0` while the list was `FtpPi`, `NxProfile`,
  `txid-edit`. That first reading seemed to say the field is not an index of anything. **Section 6 corrects
  this**: with a clean test the field is the index of the *selected* profile (it followed the selected
  profile as other profiles were inserted and reordered). One reading, at 00:55, still does not fit.

- **Durability:** a profile added with `(count, 1)` was still there, byte-identical (SHA-256 matched), after
  the owner powered the camera off from its menu and on again.
- **Wi-Fi restart after a write is not established for this client.** NX Field's writes were each followed
  by the camera re-associating with the access point (section 2). After this client's writes, port 15740
  was open again at the first check, 3 s later, and the station's connected time was not recorded. A
  refused write (`0x201F`) left the camera running and the connection numbers advancing normally. Any
  NX Field session on the phone was dropped each time this client connected, as already noted.

### What went wrong, so it is not repeated

- **`0x90EE(0xFFFFFFFF, 0)` hung the camera.** It never replied; afterwards the camera stayed associated
  with the access point but stopped answering ARP and ping, and port 15740 stayed closed for over a
  minute. The camera showed a "wireless error" (the owner read it as `1f`; not matched to anything). A power
  cycle cleared it. **Do not send out-of-range slot numbers other than the values in the table.**
- **The power cycle after the hang reverted the profile list** to an earlier state: slot 0 `NxProfile`
  with the `fleet` login NX Field had written (331 B), slot 1 `txid-edit`, count 2. Lost: the owner's
  menu edit of the login, all the writes made over Wi-Fi that day, and the profile that had been added.
  The camera then failed with error 32 because the Pi has no `fleet` account. The earlier NX Field
  write had survived. Cause unknown; one guess is that the hang left changes uncommitted and the reboot
  loaded the last committed copy. Restoring the profile with `--write` and a later clean power-off both
  held.
- **Ordering assumption:** a first replacement test was meant to be a no-op and instead swapped two profiles,
  because writing any slot moves it to the front. Restore with a write of the other profile.

## 6. The same ops over USB, the selection field, and the rest of the FTP op range

Done later the same evening on the same Z9 (`Z_9_3023668`), with `vendor-read-ftp`,
`vendor-write-ftp-blob` and a new `vendor-probe-op` (below).

### Reads and the `00` tag byte

- `vendor-read-ftp --slot N` reads every stored profile over USB. The first USB run reported
  `count 3, limit 20, slot 0`.
- The Rust decoder rejected two of the blobs: byte 105, the first byte of the 2.4 GHz tag, was `00` where
  every earlier blob had `03` (`03 00 xx 00`). It is `00` in `NxProfile` and `FtpPi` and `03` in
  `txid-edit`. `NxProfile` descends from `SNOOP`, which was made in the camera's own menu and then
  rewritten by NX Field; `txid-edit` was written earlier by this tool's encoder. That is the only
  difference found, so "camera-menu lineage gives `00`" is a guess; the byte's meaning is unknown. The
  decoder now accepts `03` or `00`, keeps the byte as `ssid24_lead`, and `as_encodable()` refuses `00`
  (the encoder writes `03`). Anything else is still rejected with the offset.

### Adding over USB

`vendor-write-ftp-blob --file F --slot COUNT --param2 1` does the same add as over Wi-Fi: count 3 → 4,
new profile (`UsbPi`) at the front. The tool already allowed `slot == count`; its dry run called the add
"untested".

### The `0x90EB` slot field is the selected profile's index

- After `UsbPi` was added at the front of `[NxProfile, FtpPi, txid-edit]`, the field read `1`. It had read
  `0` when `NxProfile` was first, so it had followed `NxProfile`.
- The owner then picked `FtpPi` in the camera's FTP menu. Result: `FtpPi` moved to the front of the list
  and the field read `0`. **Selecting a profile in the menu moves it to the front.**
- Then `0x90EE(2, 0)` was sent with `NxProfile`'s own bytes (it was at index 2). `NxProfile` moved to the
  front and the field read `1`, pointing at `FtpPi`, which had been pushed down one place. **A write
  moves its profile to the front but does not change the selection.**
- After a delete-all (below) the field reads `0xFF`, and re-adding profiles leaves it at `0xFF`: adds do
  not select. How to select one over the wire is still unknown.

### Probing the unused ops in the range

`vendor-probe-op OP [--param N]... [--data HEX] [--setup]` sends one op (with optional data-out of up to
32 bytes) from a fixed list (`0x90E9`,
`0x90EA`, `0x90EC`, `0x90EF`, and `0x9FFF` as a control that should not exist), at most two parameters
and never `0xFFFFFFFF`. It reads the profile list and the selected index before and after. If the op gets
no reply the session is unusable, so the "after" listing is then unavailable and `vendor-read-ftp` is run
again. Results, Z9:

| Call | Result |
|---|---|
| `0x9FFF`, bare or after the `0x90E8` setup | `0x2005` OperationNotSupported at once; list unchanged. What a nonexistent op looks like. |
| **`0x90EA`, bare** | `0x2001` OK. **All profiles deleted**: count 4 → 0, selected index `0xFF`. |
| **`0x90EF(1)`** | `0x2001` OK. **The profile at index 1 was deleted** (count 4 → 3, the others kept their order). |
| `0x90EF`, bare | USB pipe stall (`Pipe error`). Camera fine afterwards. The same way a bad slot number was refused over USB. |
| `0x90EC`, bare, or `0x90EC(1)` with no data | no reply, USB timeout. Camera answered again immediately in a new session. |
| **`0x90EC` with data-out `01`, `01 00 00 00` or `01 00 00`** (no parameters) | `0x2001` OK each time; profile list and selection unchanged. **Afterwards the `0x90EB` device-name string was empty** (see below). |
| `0x90EC(1)` with data-out `01` | USB pipe stall. Camera fine afterwards. |
| `0x90EC` with data-out `0c 5a 00 5f 00 39 00 5f 00 33 00 30 00 32 00 33 00 36 00 36 00 38 00 00 00` (the original name as a PTP string), then `02 41 00 00` and `02 41 00 00 00` (`"A"`) | `0x2001` OK each time; the name stayed empty. |
| `0x90E9`, bare, after the setup, over Wi-Fi | no reply within 8 s, and then the camera answered nothing on Wi-Fi (port 15740 closed) or on USB (every transaction timed out) until the owner power-cycled it. |
| **`0x90E9` with data-out** `01`, `01 00 00 00`, `01 00 00` (after the setup), and `00`; no parameters; over Wi-Fi | `0x2001` OK each time. The whole `0x90EB` reply (including the empty device name) and the profile names were byte-for-byte unchanged afterwards. The owner saw no difference in the camera's FTP menu, and two shots taken after the `01` and `00` payloads were each uploaded as usual (`_BLW9416.JPG`, `_BLW9417.JPG`, deleted afterwards). So `0x90E9` takes data, but no visible effect was found. |

- **`0x90EA` and `0x90EF(n)` are the delete operations** (all, and one). They are destructive and run
  with no confirmation. The four profiles were restored from saved raw blobs: re-added in reverse order
  (each add goes to the front) and compared byte-for-byte with the originals.
- `0x90E9` and `0x90EC` exist (they do not answer `0x2005`) but sent bare they get no reply. Sent bare,
  `0x90E9` left the camera's PTP engine stuck on both transports (probably the unanswered transaction,
  unverified); with data-out it answers OK and the camera stays responsive. Both take data-out and no
  parameters. `0x90E9` showed no effect anywhere we looked; `0x90EC` blanked the device-name string.
- **An unintended change.** The `0x90EB` reply is `c8 00`, count, limit, selected index, a 16-byte GUID, a
  PTP string, `f4 01`. The string was `Z_9_3023668` (model and serial) before the three `0x90EC` data runs
  and is `01 00 00` (an empty string) after them; the GUID did not change. It stayed empty through a power
  cycle and after the later restore attempts, so it is stored. Which of the three payloads blanked it is
  not known (the list was not re-read between them), and what `0x90EC` really sets is not known: it did not
  accept a well-formed name. The camera still joined the access point, logged in over SFTP and uploaded a
  JPEG (`_BLW9415.JPG`, 978,248 bytes) afterwards, and NX Field's port was open, so the empty name has not
  been seen to matter. The original value is recorded here in case it does.
- **No op that selects a profile was found.** NX Field sends none of `0x90E9`, `0x90EA`, `0x90EC`,
  `0x90EF` in any capture (July sessions, or the 186 MB one here), and the SDK's capability list
  (`MaidLayer.config`, the TypeCommon module's strings) has nothing for FTP profiles. Selection may be
  menu-only.

### Operation map (Z9, this evening)

| Op | Effect |
|---|---|
| `0x90E8` | setup, data-out `01 00 00`, sent before the others |
| `0x90EB` | info: count, limit 20, selected index (`0xFF` = none) |
| `0x90ED(n)` | read profile n |
| `0x90EE(n, 0)` | replace profile n, move it to the front |
| `0x90EE(count, 1)` | add a profile at the front |
| `0x90EF(n)` | delete profile n |
| `0x90EA` | delete all profiles |
| `0x90EC` + data | accepted with 1, 3 or 4 bytes; blanked the `0x90EB` device-name string (cause unclear); stalls if given a parameter |
| `0x90E9` + data | accepted with 1 or 4 bytes (`01`, `01 00 00 00`, `01 00 00`, `00`); no visible effect found; sent bare it wedged the camera until a power cycle |

## 7. Open questions

- Which camera models accept this client and the probes. Only the Z9 above was tried. The Z6III answered
  the read ops over USB earlier but has not been probed.
- How to select a profile over the wire, if it can be done at all. `0x90E9` and `0x90EC` take data-out of
  an unknown format and neither selected anything with the short payloads tried. Further guessing risks
  another stored change (`0x90EC` blanked the device name); a capture of the camera's own menu activity
  (changing an FTP setting, selecting a profile) over Wi-Fi would show whether any of these ops is ever
  sent.
- What the `0x90EB` device-name string is for, why it blanked, and how to set it back to
  `Z_9_3023668`.
- What the second parameter of `0x90EE` means beyond 0 (replace) and 1 (add), and what the `0x90E8`
  setup byte (always `01 00 00`) selects. Neither was varied.
- What the `00` / `03` byte in the 2.4 GHz tag means.
- Whether the reverted profile list after the first power cycle was the hang or a normal "commit on
  shutdown", and the unexplained 00:55 slot-field reading.
- Whether NX Field defaults to SFTP/22, and whether its form really prefills from the read.
- What `ObjectAdded` pairs mean on a dual-slot camera.
- Whether the Wi-Fi restart after a save is the save or the unreachable host, and whether writes sent by
  the Wi-Fi client cause one at all (record the station's connected time before and after).
- Porting the PTP/IP client into the Rust tool (something like `vendor-read-ftp --wifi IP`). The USB
  commands already cover read, add, replace, and (through `vendor-probe-op`) delete; there is no
  dedicated delete command, and `vendor-probe-op` is deliberately restricted to a fixed op list.

## 8. What the Rust library now enforces (2026-11-05, not run on hardware)

Everything above came from experiments; `src/ptp_usb.rs` now turns the safe part of it into a gate and a read-back.

- **Only two write forms are sent.** `write_ftp_profile_blob` (and its `_locked` form) accepts `0x90EE(n, 0)` with `n` below the
  stored count (replace) and `0x90EE(count, 1)` (add). Everything else is `PtpUsbError::FtpWriteRefused` with a typed
  `FtpWriteRefusal`: a second parameter other than 0 or 1 (`UnknownParam2`), slot `0xFFFFFFFF` (`SlotHangsCamera`), a replace
  of a slot that is not stored (`ReplaceSlotNotStored`), an add at any slot but the count (`AddNotAtCount`; `(n < count, 1)` was
  never tried). The stored count is read **in the write's own session** (`0x90E8`, `0x90EB`, then the write's own `0x90E8`,
  `0x90EE`), so the count cannot change between the check and the write. That read-only pair ahead of a write has not been
  run on a camera. `write_ftp_profile` (encoder path, fixed `[0, 0]`) is unchanged and unguarded; the read-back variants below
  route it through the guard.
- **Read-back.** `write_ftp_profile_blob_verified` / `write_ftp_profile_verified` (and `_locked` forms) read slot 0 back after the
  write (`0x90E8`, `0x90EB`, `0x90ED(0)`) and compare the decoded profile, field by field, with the one sent, and the count
  with `count_before` (+1 for an add). The result is `FtpWriteVerification`: `Verified`, `Mismatch` (names of the differing
  fields, never values), `ReadBackUndecodable`, `ReadBackFailed`. It depends on two Z 9 facts from section 5 and 6: a successful
  write moves the profile to slot 0, and an add raises the count by one. **A mismatch on a body that does not move the profile to
  the front would be a false alarm; on the Z6III this is untested.** The comparison is of decoded profiles, not bytes, because
  the camera re-serialises an empty folder (handoff doc, the option-flags table). The read-back inside the write's session is
  untested; reading slot 0 in a separate process after a write was done by hand.
- **Non-ASCII text.** The u32-length fields (both SSID fields, username, password, the IPv6 placeholders) are read as UTF-8, so a
  profile with a Japanese SSID decodes, displays and can be copied with `vendor-write-ftp-blob`. No capture contains non-ASCII
  content; UTF-8 is assumed by analogy with the IPTC fields of the same shape. Bytes that are not UTF-8 (for example a Latin-1
  SSID) are still refused, naming the field and the first bad byte, and the raw bytes remain available. The encoder still writes
  these fields as ASCII only, so `as_encodable()` is `None` for such a profile.
- **Artist and Copyright** writes (`vendor-set-string 0xD072/0xD073`) now need `--force-unconfirmed-model` like an unconfirmed
  model: only the image comment was ever written.

### What the CLI does with it (b7d3 remediation, 2026-11-05; the read-back has not been run on a camera)

- **`vendor-write-ftp-blob`** no longer checks the count itself. It asks the library for a plan (`plan_ftp_write_locked`: the model
  gate, the blob must decode, the two allowed forms, the count read in that session; nothing is written), prints it, and a
  `--dry-run` stops there, so a dry run now fails exactly where the write would, including on a Z 5. A real write then goes through
  `write_ftp_profile_blob_verified_locked` (the count is read again in the write's own session) and prints the read-back.
  The plan and the result say where the profile lands: a replace of slot N moves it to the **front** (slot 0) and shifts the
  profiles that were in front of it down; an add puts the new profile at the front and raises the count by one. Do not look
  for the written profile in slot N afterwards. `--slot COUNT` with the default `--param2 0` is refused (the 2026-11-05 Z 9
  experiment: `0x90EE(1, 0)` at count 1 sent the data, stalled the pipe and added nothing); an add is
  `--slot COUNT --param2 1`.
- **Exit status.** 0 is a write whose read-back showed slot 0 equal to what was sent and the count the write should leave.
  If the data was sent and the write then failed, or the camera answered OK but the read-back did not match, did not decode or
  could not be read, the status is **4** (the camera MAY have changed). A mismatch on a body that does not put a written
  profile at slot 0 would be reported the same way (a Z6III is untested). Read the list (`vendor-read-ftp --slot 0`) before
  repeating the write: a repeated add stores a duplicate and a repeated replace of a slot above 0 hits a different profile,
  because the first one reordered the list. The old text that called a repeat "idempotent" was wrong for both.
- **`vendor-read-ftp --raw`** zeroes the password and the 5 GHz field in the hex dump wherever they appear: first by the
  decoder's own offsets, then every occurrence of their text anywhere in the blob (also as UTF-16, the encoding of the profile
  name), overlapping copies included. Before this only the first occurrence was zeroed, and a username equal to the password
  left the real password in the "masked" dump. The decoded field lines still print the profile name, SSIDs, host and username as
  they are, so a password that is the same text as one of those is visible there.
- **`vendor-write-ftp --port`** is the u16 written after the password (8080 in every camera blob; the decoder's `second_port`),
  not the FTP port, which the encoder fixes at 21. The success line says so. The default stays 21 (a change to what is written
  needs a hardware run); the cameras read so far carry 8080.
- **`vendor-set-string --dry-run`** runs the same checks as the write (`plan_string_property_locked`: the property, the model and
  property gates, Latin-1 and control characters, a read-write descriptor) and fails where the write would.

## Credentials

The FTP login above is a throwaway lab login already recorded in earlier notes. The Pi's own login is
deliberately not written here. Profile passwords are masked everywhere the tools print them.
