# RPi5 AP Capture Setup

Alternative to the ARP-MITM setup in `docs/ptpip-capture-notes.md`. Instead of
spoofing ARP on someone else's router to force traffic through the Mac, the
RPi5 *is* the AP the camera and phone associate to. Its onboard `brcmfmac`
WiFi driver has no hardware-bridging fastpath in AP mode (unlike the Beryl
AX's MediaTek MT7981), so `tcpdump` on `wlan0` sees all client<->client
traffic for free — no ARP spoofing, no bettercap, no "disconnect before
starting MITM" dance.

Bootstrap script: `scripts/rpi-ap-setup.sh`.

## 1. Flash the SD card

Raspberry Pi Imager → CHOOSE DEVICE → Raspberry Pi 5 → CHOOSE OS →
**Raspberry Pi OS Lite (64-bit)** (headless, no desktop needed).

Before writing, open OS customisation (gear icon / Ctrl+Shift+X):

- **General tab**: set hostname (e.g. `nikon-snoop`), username `pi` (or your
  own) with a password you'll remember.
- **Services tab**: enable SSH, "Allow public-key authentication only" and
  paste your pubkey (`~/.ssh/id_ed25519.pub` or similar) if you want
  passwordless login. Password auth is fine too for a one-off box.
- **Leave the WiFi section blank.** wlan0 is going to run its own AP, not
  join a network — configuring it here would fight the bootstrap script.

Write, eject, insert into the RPi5.

## 2. First boot — management access

The RPi5 has no WiFi client config, so reach it over **Ethernet**: plug it
into your existing LAN, power on, wait ~30s, then:

```bash
ssh pi@nikon-snoop.local
```

(mDNS hostname resolution; use `.local` per the hostname you set. If that
doesn't resolve, check your router's DHCP lease list for the IP instead.)

## 3. Run the bootstrap script

Copy the script over and run it:

```bash
scp scripts/rpi-ap-setup.sh pi@nikon-snoop.local:~
ssh pi@nikon-snoop.local
sudo AP_PASSPHRASE='your-passphrase-here' AP_COUNTRY_CODE=US ./rpi-ap-setup.sh
```

Override any of `AP_SSID`, `AP_PASSPHRASE`, `AP_IFACE`, `AP_IP`,
`AP_COUNTRY_CODE`, `AP_CHANNEL`, `AP_HW_MODE` via environment variables (see
the top of the script for defaults). `AP_COUNTRY_CODE` matters for legal
5GHz channel/power — set it to your actual regulatory domain.

Defaults: SSID `nikon-snoop`, 5GHz channel 36 (UNII-1, no DFS/radar wait),
AP at `192.168.66.1/24`, DHCP `192.168.66.10`–`.100`, isolated (no internet
uplink — Ethernet stays management-only).

## 4. Verify

```bash
iw dev wlan0 info          # type should read "AP"
sudo systemctl status hostapd dnsmasq
journalctl -u hostapd -n 50
```

Associate the phone and camera to SSID `nikon-snoop`. NX Field may prompt to
"stay connected, no internet" — accept it; that's expected on the isolated
network.

## 5. Capture

```bash
sudo tcpdump -i wlan0 -s0 -w ~/captures/session-$(date +%Y%m%d-%H%M%S).pcap
```

Or `tshark -i wlan0` for live decode. Same PTP/IP analysis steps as
`docs/ptpip-capture-notes.md` §Next session goals apply — filter `ptpip` in
Wireshark, look for `0x1016` SetDevicePropValue, `0x1014` GetDevicePropDesc,
`0x9xxx` vendor ops.

No ARP-spoof setup/teardown is needed — this is just `tcpdump` on the
interface that's already carrying everything.

## Caveats / fallbacks

- **5GHz AP mode on the onboard chip**: known to work on RPi4's BCM43455 via
  hostapd+nl80211; RPi5 uses the same family. If `hostapd` fails to bring up
  `hw_mode=a` reliably, fall back to `AP_HW_MODE=g` (2.4GHz) — note the
  camera/phone were previously captured on 5GHz only
  (`docs/ptpip-capture-notes.md`), so confirm they'll actually associate on
  2.4GHz before relying on this path. A USB WiFi adapter with confirmed
  AP-mode support (e.g. RTL8812AU/MT7612U based) is the other fallback if
  the onboard radio is flaky in AP mode.
- **DFS channels** (52–144 in most domains) require a radar Channel
  Availability Check before the AP comes up (~1 min delay) and can force a
  channel switch if radar is detected. Channel 36 avoids this entirely —
  don't change `AP_CHANNEL` into the DFS range without expecting that delay.
- **mDNS discovery**: both clients are on the same L2 segment (no VLANs/subnet
  crossing), so the `_ptp._tcp.local.` advertisement from
  `docs/ptpip-capture-notes.md` should work unmodified — no reflector needed.
