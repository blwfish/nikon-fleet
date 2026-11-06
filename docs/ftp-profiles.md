# Managing FTP profiles from this computer

The camera's own menu for FTP profiles checks nothing as you type and throws everything away on one error. This does
the typing and the checking on the computer: edit a copy of a snapshot (`docs/snapshot-editing.md`), test the server,
then restore the result to the body with `restore-extended` (a guarded plan, a restore point, a read-back). The restore
replaces the body's whole list: it deletes the stored profiles one at a time (`0x90EF(n)`, highest index first, the
count checked after each; there is no delete-all in the product path) and then adds the snapshot's, last first. That
sequence is unverified on hardware (`docs/extended-snapshot.md`, "Not yet run on hardware").

Code: `src/ftp_edit.rs` (fields), `ptp_usb::encode_decoded_ftp_profile` (the writer), `src/ftp_test.rs` (the test).
**Neither the fields nor the test have touched a camera yet, and the window has not been seen on a screen** (see "Not verified").

## The fields

| field | meaning | CLI |
|---|---|---|
| name | shown in the camera's menu | `--name` |
| Wi-Fi name | the field the encoder calls the 2.4 GHz SSID | `--wifi-name` |
| Wi-Fi key | stored in what the encoder calls the 5 GHz SSID field; **probably** a passphrase, **unconfirmed** (never checked on a camera) | `--wifi-key` |
| host, folder, login, password | the server | `--host`, `--folder`, `--user`, `--password` / `--password-stdin` |
| port | the server's port (the field before the user name) | `--port` |
| protocol | FTP or SFTP: option bit `0x0008`, confirmed on one profile | `--protocol ftp\|sftp` |
| second port | probably a proxy port; 8080 in nearly every profile seen | `--proxy-port` |

Everything else in the profile (the camera's own address, timestamps, tag bytes, the other option bits) is kept as the
camera wrote it. `edit ftp-set FILE INDEX …`, `ftp-add FILE --name N --host H … [--at POS]`, `ftp-duplicate FILE INDEX
[--name N] …` (the copy goes right after the original and keeps its Wi-Fi name and key and login: most profiles share
the network), `ftp-show FILE INDEX [--reveal]`, and the earlier `ftp-delete` / `ftp-move`.

Checks: the encoder's own limits and character rules (the Wi-Fi name 32 bytes, the host 199 units, ASCII only where
the wire form is bytes, no control characters), a profile needs a name and a server, a port is 1 to 65535, the list
holds at most the camera's limit (20). The edit log names the fields changed and never a value.

## In the GUI

In an edited copy's window (open a snapshot with **Edit a copy…**), the **Owner data (edit)** section lists the FTP
profiles. For each: its position and name; **↑ ↓** to move it, **Duplicate** (a copy right after it, with its Wi-Fi
name and key and login), **Delete** (a second press on the same row confirms), and **Test connection**. A
**fields** fold-out holds the name, server, folder, login, password, Wi-Fi name, Wi-Fi key, server port, protocol
(FTP or SFTP) and second port. A field is saved when you leave its box (Enter, Tab or a click elsewhere; Escape gives up),
through the same checks as the command line, and a refusal says why and puts the stored value back. Passwords and the
Wi-Fi key are masked; **👁 Show passwords** at the top of the section reveals them. **Add a profile** takes a name and a
server (and, if you like, folder, login, password and Wi-Fi) and starts from the editor's template: FTP port 21, second port
8080, option bits `0x0005`, and **no stored camera address** (it used to carry the capture rig's, 192.168.66.87/24; see
`docs/snapshot-editing.md`). Duplicate copies the profile whole, its camera address included.

**Test connection** runs `ftp-test`'s engine on a thread of its own with the profile as stored in the file (so what is
tested is what was saved), and shows each step under the profile: ✔ / ✘ / – with the server's words and a hint on a
failure, the SFTP host key, and a warning naming any test file that could not be deleted. The checkbox above the list
leaves the test file out. A result is of the profile as it was: any change to the file clears the results.
It says in the window that it tests the server only.

## The writer

`encode_decoded_ftp_profile` walks the same layout table the strict decoder reads, with every value taken from the
decoded profile. A profile that is read and not changed is written back **byte for byte**: pinned for the six real
blobs in `tests/fixtures/ftp_profile_vectors.json` and, ad hoc, for 18 profiles read from a Z 9 and a Z6III
(`NIKON_FLEET_ROUNDTRIP_DIR=<data dir> cargo test -p nikon-fleet --test ftp_profile_roundtrip -- --ignored`). The older
`encode_ftp_profile` still exists and still fixes the server port to 21, the folder to empty and the option bits.

## The test: `ftp-test`

```bash
nikon-fleet ftp-test --snapshot FILE --profile N        # a profile stored in a snapshot (read only)
nikon-fleet ftp-test --host H --user U --password-stdin [--port P] [--protocol sftp] [--folder F]
```

Steps, each reported with its outcome: look the host up, connect, (SFTP) the SSH handshake, log in, go to the folder,
and, unless `--no-file-check`, put a small uniquely-named file there, fetch it back, compare it and delete it. A failure
names the step, the address and port tried, the server's own words and a hint in terms of the profile; a file that
could not be deleted is named so it can be removed by hand. Exit status 0 only if every step passed. `--json` prints
the report. FTP transfers are set to binary type (the client's default, ASCII, rewrites line endings); that the
camera itself sends binary is an assumption, not something checked.

The last line says what was tried: with `--no-file-check` a pass reads "the login and the folder passed; no test file was
put, so writing to the folder is not tested", and only a run that also put, fetched, compared and deleted the file reads
"the server accepted this profile". After a failed upload that may have left part of a file (a timeout, an aborted
transfer; not a plain refusal) the test still tries to delete it, and names it if it cannot.

**The report never holds the password**: it is not stored, and any copy of it in text the server or the client library
produced (a server may echo it in a refusal) is replaced by `<password hidden>`. A server that echoes it altered is not
caught. A failure keeps the server's own words; when the server answered with an FTP reply code the code is also a field
(`reply_code` in the JSON), and what a code means goes through one table in `src/ftp_test.rs` (`classify_reply`): 530 at
the login is a refused login, 421 is a server that will not serve right now (not a wrong password), 550 at the folder is a
missing folder, and a code the table does not list is "other" with the server's words.

**Time limits.** `--timeout` is seconds per step, and per address while connecting (a name with several addresses can take
that long for each). For FTP it covers the connection, every read and write on the control connection, and the data
connection (connecting to the passive address and each read and write on it), which the client library does not limit
by itself. The window's worker has no cancel; it ends when the steps do. A data address the server announces that no
packet can reach is waited on for at most the timeout. The test file is read back with a size limit (64 KiB); more than
that is reported as a data mismatch.

What it cannot tell: a wrong host name and a server on a network this computer is not on both look like "could not
reach", and the hint says so. It tests the server side only; whether the camera can join the Wi-Fi network is not
tested. Plain FTP only (the profile has no known FTPS setting). The SFTP host key is reported (`SHA256:…`), not checked.
SFTP logs in with a password only.

Clients: `suppaftp` without TLS for FTP, `russh` and `russh-sftp` (pure Rust, no libssh2 or OpenSSL) for SFTP, driven by a
current-thread tokio runtime the test creates for itself. About 60 crates are new to the project.

## Not verified

- What a Z 9 or Z6III does with a profile whose port, folder or protocol was changed, or a brand-new one built from the
  template. The check is to restore such a copy to a body and read the profile back (`vendor-read-ftp`).
- The template's **no stored camera address** (0.0.0.0/0, 2.4 GHz tag kind `0x30`; before, the capture rig's 192.168.66.87/24).
  Evidence is only that one camera-made fixture (`hotspot_no_address`) has that form; the combination with option bits
  `0x0005`, 5 GHz kind 4 and second port 8080 is not itself a camera-made one. The check on a body: add a profile in the
  editor, restore it, and see whether the camera lists it and can connect with it.
- Which option bit is passive mode, and whether any is FTPS: change one menu setting at a time and compare
  `vendor-read-ftp` (see `docs/property-sweep.md`).
- The test against a real server. Nothing here has been run against a real FTP or SFTP server; both protocols are tested
  only against servers written for the tests. Those servers behave as this program was written to expect: the FTP one
  rewrites line endings in ASCII type and records `TYPE I`, so dropping the binary setting fails a test, and one that
  accepts and never speaks must time out; how real servers differ (reply wording, ASCII/binary handling, passive-mode
  address announcements, what they list for a login that may write but not read) is unknown.
- How an SFTP server that offers only keys, or one that disconnects during the handshake, is reported by `russh`: the
  kinds were set from the library's documented errors and from the test servers, not from real ones.
- Whether a write-only upload folder passes the SFTP folder check, which lists the folder.
