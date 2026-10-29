# Windows smoke test (instructions for Vic's Claude)

**This file is addressed to you, the AI assistant on Vic's Windows computer, not to Vic.** Vic is a
mechanical engineer, not a software engineer: do not expect them to run `git`, `cargo` or PowerShell
commands, read build output, or know what a driver is. You do the technical work. Vic's part is
physical and decisional: plug a camera in, switch it on, tell you which body it is, close Nikon's
programs, look at the screen when a step says so, and say yes or no when you ask permission. Talk to
them in plain language, say what you are about to do and why before you do it, and keep what you ask
of them to one small thing at a time.

You are helping find out whether nikon-fleet works on **Windows**. It was written and tested on macOS.
Until the `windows` CI job existed (2026-10-29) nothing had compiled it for Windows, and **no camera has
ever been driven from Windows**. Read all of this first, do it in order, and write down exactly what
happened. Where something fails, the exact output is the useful part; a fix is not expected from you.

## The cameras

Vic has several Nikon bodies: **Z30 (two), Z6II and Z8**. None of these four has ever been read by this
project (it knows the Z 5, Z 9 and Z6III). Every camera step below is run **once per body**, one body at
a time, and the report is per body (model string exactly as printed, firmware, result of each step).

- The **Z30** question: nobody has been able to check whether a Z30 reports a serial number over USB,
  or whether it matches the USB descriptor's. Step 4 (`device-info`) answers it.
- For all four, the tool will say `known_model: false` and "no baseline" (expected, not an error; it
  means nobody has yet recorded what a complete capture of that model looks like). A capture should
  still work. A *refused* capture, or a very small property count (the Z 5 gave 165, the Z 9 196, the
  Z6III 200), is a finding: report it verbatim.
- Also record, for each body, the **USB product string, idProduct and bcdDevice** from Device Manager
  (the camera's Details tab, Hardware Ids; `VID_04B0&PID_xxxx` is the vendor and product id). The
  maintainer adds a model to the project's registry only from a real probe, never from a guess.
- The two Z30 bodies are a chance to compare: same model, two serials. Note whether they differ only in
  the serial.

## Rules

- **Read-only for the camera.** Never run `restore`, `transplant`, `vendor-write-ftp`, `firmware add`
  or `firmware pin`, and never use a write button in the GUI. You may run `discover`, `snapshot`,
  `device-info`, `vendor-read`, `check`, `diff`, `ls`, `show` and `settings show`.
- **Ask Vic before installing any software**, and say what it is, roughly how big, and whether it
  needs administrator rights (Rust, the Visual Studio Build Tools and Python do; the Build Tools are
  several gigabytes). Do it for them once they agree. **Do not change system settings or install
  drivers yourself.** Binding a different USB driver
  (Zadig and the like) replaces what Windows, NX Studio and Nikon's own software use for the camera.
  If a step needs that, describe it to the person, say what it affects, and let them decide.
- **Do not edit the code.** If something is wrong, put it in the report.
- **One camera attached at a time.** If two Nikon bodies are attached, stop and ask which to use. When
  you move to the next body, ask Vic to switch the first off and unplug it before connecting the next.
- **Snapshots and `device-info` output contain the camera's serial number.** Keep snapshots on this
  machine, outside any git repository. Do not send or commit them. Say plainly in the report which
  excerpts contain a serial.
- **Close NX Studio, Nikon Transfer and anything else that talks to the camera** before the camera
  steps; they can hold it. If a command says `camera_busy` or cannot claim the interface, say which
  programs were running.
- If a step fails, **stop and report the exact output**, then continue with the next step that does
  not depend on it. Do not work around a failure with extra flags unless this file says to.
- The MCP server (`fleet-mcp`) is macOS and Linux only. It is not part of this test.

## What is known and what is not

Known (from the CI runs of 2026-10-29): the library, the CLI (`nikon-fleet.exe`) and the egui GUI
(`fleet-gui.exe`) **compile** on Windows, and most of their tests pass. The camera lock (the
cross-process mutex that stops two programs using the camera at once) was added for Windows that day
with `LockFileEx`, and has only been unit-tested.

Not known, and the reason for this test:

1. **How the camera shows up on Windows USB.** The code talks to the camera through `rusb`
   (libusb). On Windows, libusb can only open a device whose driver is WinUSB. A camera in its normal
   mode uses Windows' own PTP/MTP driver. Expect `device-info` and `vendor-read` to fail to open the
   camera unless a WinUSB driver is bound. This is the single most important thing to learn: **what
   exact error, and what does Device Manager say the driver is.**
2. **Whether the Nikon SDK works from here.** `discover`, `snapshot`, `check` and the rest load the
   SDK's `ControlServiceLayer.dll` (and `NkdPTP.dll`, `NkRoyalmile.dll`, `dnssd.dll`) through the
   SDK's own driver stack, not libusb. This path has never been run on Windows.
3. **Paths.** A `/` or `\` in output, a drive letter, spaces in a path (the SDK folder and the data
   directory are the likely places).
4. **The GUI**, including the snapshot viewer opening as its own window.

## 0. What you need

- Windows 10 or 11 (64-bit), the output of `ver` and `systeminfo | findstr /B /C:"OS"` for the report.
- **Rust 1.89 or newer** (`rustup`; the code needs `File::try_lock`) and the **MSVC build tools**
  ("Desktop development with C++" in the Visual Studio Build Tools installer). `git`.
- The Nikon SDK for Z cameras (`S-SDKZ-200BF-ALLIN`), unpacked. It is not redistributable and is not
  in the repository; the person has it or can download it from Nikon's developer site. Ask which folder.
- A Nikon Z body (start with whichever Vic has to hand), on, connected by USB, in normal shooting
  state, with its USB cable and a port on the computer itself (not through a hub).
- Optional, for display names: Python 3.9+ and `pdftotext` (Poppler for Windows) for step 3. Skip if
  they are not installed; everything else works without them.

Use a scratch directory outside the repository for snapshots, for example `%USERPROFILE%\nikon-verify`,
as `--data-dir` below. Run everything from PowerShell.

## 1. Get it and build it

(You do this part; Vic only has to say yes to the installs. If Rust or `git` is missing, offer to
install them. If you can write and run the commands yourself, do not ask Vic to type any of it.)

```powershell
git clone https://github.com/blwfish/nikon-fleet.git
cd nikon-fleet
rustc --version ; cargo --version
cargo build --release -p nikon-fleet -p fleet-gui
```

Record whether the build passed. It takes several minutes. If it fails, paste the first error and stop.
The programs are `target\release\nikon-fleet.exe` and `target\release\fleet-gui.exe`; the GUI looks
for `nikon-fleet.exe` next to itself.

Then run the tests and record the summary lines (`test result: ... passed; N failed`):

```powershell
$env:CI = "0"
cargo test --release -p nikon-fleet -p fleet-gui --no-fail-fast 2>&1 | Select-String "test result|FAILED|panicked"
```

The CI job runs the same on a clean machine and expects them all to pass; a failure here that CI did
not have points at this machine (path with spaces, a different locale, antivirus holding a file).

## 2. Stage the SDK runtime

```powershell
scripts\setup-sdk-runtime.ps1 C:\path\to\S-SDKZ-200BF-ALLIN
```

(Run it with `powershell -ExecutionPolicy Bypass -File scripts\setup-sdk-runtime.ps1 <folder>` if the
policy blocks it; tell the person you are doing that.) It copies the four DLLs and the config files
into `sdk-runtime\`. Record the output. The script has never been run; any error is a finding.

## 3. Names for `show` (optional)

```powershell
python scripts\extract-maid-labels.py C:\path\to\S-SDKZ-200BF-ALLIN
```

Needs `pdftotext` on PATH. Writes `sdk-runtime\maid-labels.json`. Skip if either is missing.

## 4. Read-only camera step without the SDK: `device-info`

This is the raw USB path (libusb). It answers two questions at once: does the libusb path work on
Windows at all, and what serial number does the camera report.

```powershell
target\release\nikon-fleet.exe device-info
target\release\nikon-fleet.exe device-info --json
```

Expected on a machine where it works: a block of DeviceInfo fields and a last line

```
serial: PTP "<x>" | USB "<y>" | match|differ|ptp_empty|usb_empty|both_empty
```

For the report, give the **model, firmware and the verdict word** (`match`, `differ`, `usb_empty`,
...), and whether the PTP serial is zero-padded to 32 characters. Do not paste the serial itself unless
the person says it is fine. For a **Z30** the verdict word is the answer the maintainer needs.

If it fails, the exact error matters most. Likely forms: no device found, "could not claim the PTP USB
interface", an access or driver error. Then collect, for the report:

```powershell
Get-PnpDevice -PresentOnly | Where-Object { $_.FriendlyName -match "Nikon|Z ?[0-9]|Z[ _]?f|Z[ _]?fc" -or $_.InstanceId -match "VID_04B0" } | Format-List FriendlyName,InstanceId,Class,Status,Service
```

`VID_04B0` is Nikon. `Service` is the driver in use (`WinUSB` would let libusb work; `WUDFWpdMtp` or
`usbccgp` means Windows' PTP/MTP driver has it). **Do not change the driver.** Report it and let the
person decide whether to try binding WinUSB for this camera (it makes Explorer and NX Studio stop seeing
it until it is switched back, and may conflict with the SDK's own driver in step 5).

## 5. Camera steps through the SDK

Only if step 2 succeeded.

```powershell
target\release\nikon-fleet.exe discover
target\release\nikon-fleet.exe discover --json
```

Record the model, firmware and sdk id for each camera, `known_model` and `baseline`. Not the serial.
Then a capture, which only writes a file on this computer:

```powershell
target\release\nikon-fleet.exe --data-dir $env:USERPROFILE\nikon-verify snapshot --label windows-smoke
target\release\nikon-fleet.exe --data-dir $env:USERPROFILE\nikon-verify ls
target\release\nikon-fleet.exe --data-dir $env:USERPROFILE\nikon-verify show <the file name ls printed>
target\release\nikon-fleet.exe --data-dir $env:USERPROFILE\nikon-verify show <file> --codes --raw
```

If `snapshot` says the SDK or `MaidLayer.config` is not found, pass `--sdk-bundle sdk-runtime\ControlServiceLayer.dll --schema sdk-runtime\MaidLayer.config` (the defaults are fixed at build time to the repository's `sdk-runtime\`) and say that you had to.

Record: did `snapshot` finish, how long it took, whether it saved the capture or refused it as
incomplete (it names the missing capabilities when it refuses), how many properties it read and how many it could not (the Z 9 reads
196 and not 11, the Z6III 200 and 12; the Z30 is unknown), and whether `show` printed readable names
and values (names need step 3). Paste 10 lines of `show` output, not the serial.

Then the lock. Start a second command while the first is running (two PowerShell windows, run `snapshot`
in one and, while it is working, `discover` in the other). The second must stop at once with
`camera busy` and the lock path, and must work normally once the first has finished. Record both. The
lock file is at `%LOCALAPPDATA%\nikon-fleet\camera.lock`.

If SDK steps hang or crash, say how long you waited, and whether `fleet-gui.exe` / `nikon-fleet.exe`
is still in Task Manager. Kill it, and unplug and replug the camera before the next step.

## 6. The GUI

```powershell
$env:NIKON_FLEET_SDK_RUNTIME = "$PWD\sdk-runtime"   # only if you run it from another directory
target\release\fleet-gui.exe
```

With the person looking at the screen, check and report each:

1. The window opens and is readable (fonts, sizing). Say if it shows a blank or black window, or only a
   console flash (a graphics problem; note the GPU).
2. **Discover** shows the camera. If it says it cannot find `nikon-fleet.exe`, that is a bug; give the
   message.
3. **Snapshot** captures and the snapshot appears in the list.
4. Double-click a snapshot: the viewer opens as **its own window** (not inside the main one), shows
   readable names and values, the code column is hidden until **Show codes** is ticked, the filter
   works, a second snapshot opens a second window, and closing a viewer leaves the others.
5. The main window's data directory (the setting) accepts a path such as `C:\Users\...` and keeps it
   after restart.
6. Closing the main window quits the program (check Task Manager that `fleet-gui.exe` and any
   `nikon-fleet.exe` are gone).

## 7. Next body

Steps 4 and 5 (and the Device Manager details from step 4) are per camera. Step 6 (the GUI) only needs
one body, with a second body if Vic is happy to repeat it. Ask Vic to switch off and unplug this body
and connect the next one, then repeat from step 4. Do the **two Z30s**, the **Z6II** and the **Z8**.
Vic may not want to do all four in one sitting: do as many as they are happy to, and say in the report
which ones were done.

## 8. Report

Write `windows-smoke-report.md` in the scratch directory (not in the repo) with, in order: the
machine (Windows version, Rust version, GPU), each numbered step with **worked / failed / skipped** and
the exact error text of any failure, the Device Manager details from step 4 if you ran them, the
`device-info` verdict word and the model/firmware of each body you saw, and anything that surprised you
that no step asked about. Remove serial numbers from anything you paste. Show Vic the file in plain
language (what worked, what did not), then tell them how to send it to Brian (blw@blw.net) by
email; the file contains no serial numbers.

Do not summarise a failure as "didn't work". The value of this test is the exact message, the exact
path, and the exact driver name.
