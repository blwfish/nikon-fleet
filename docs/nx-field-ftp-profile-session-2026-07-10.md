# NX Field FTP-profile session, and a PTP/IP profile read — 2026-07-10/11

What a live NX Field → camera session over the Raspberry Pi access point showed about Nikon FTP
profiles, plus a from-scratch PTP/IP client (`scripts/ptpip_read_ftp.py`) that reads them without USB.
Follows `docs/usb-vs-wifi-camera-behavior-handoff-2026-11-05.md` (which found the USB read op
`0x90ED` and the blob layout) and `docs/nx-field-session-2026-07-09.md`.

Everything here was observed on one camera, almost certainly the "Z9a" of the handoff doc: its slot 1
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
`0x90EB` count stayed at **2** after every write. This agrees with the USB result that `0x90EE(1, 0)`
stalls. **No way of adding a profile has been found, over USB, PTP/IP or NX Field.**

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

Reads FTP profiles over Wi-Fi with plain TCP, no NX Field, no USB. Read-only.

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

## 5. Open questions

- Which camera models accept this client. Only the one camera above was tried.
- How to add a second profile, or write to a slot other than 0 (the PTP/IP client cannot write at all;
  `vendor-write-ftp-blob` is USB-only and replaces slot 0).
- Whether NX Field defaults to SFTP/22, and whether its form really prefills from the read.
- What `ObjectAdded` pairs mean on a dual-slot camera.
- Whether the Wi-Fi restart after a save is the save or the unreachable host.
- Porting the PTP/IP read into the Rust tool (something like `vendor-read-ftp --wifi IP`). The USB
  command already has `--save-blob FILE`, added upstream while this was being written, so a Wi-Fi
  transport would reuse it.

## Credentials

The FTP login above is a throwaway lab login already recorded in earlier notes. The Pi's own login is
deliberately not written here. Profile passwords are masked everywhere the tools print them.
