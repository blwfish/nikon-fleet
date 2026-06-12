# PTP/IP Capture Session — 2026-06-12

## Network topology

| Device | IP | MAC |
|--------|-----|-----|
| Mac (MacBook Pro M1) | 192.168.8.141 | 22:7c:d7:b1:19:9c |
| iPhone 12 Pro (NX Field) | 192.168.8.183 | d6:65:c0:af:a3:aa |
| Z6III | 192.168.8.101 | 3c:be:e1:38:35:0d |
| Z9 | 192.168.8.102 | 3c:be:e1:39:7a:9d |
| Beryl router | 192.168.8.1 | 94:83:c4:7f:b0:51 |

All devices on 5GHz (rax0 radio). Cameras use static IPs (no DHCP leases).

## Discovery mechanism

Cameras advertise via mDNS `_ptp._tcp.local.` on port **15740**.

Z9 mDNS TXT record (captured):
```
guid=04b00450-0000-1001-8001-3cbee1397a9d
vid=A
pid=450
seq=20d4c27d
ver=1
apps=NCWC
grpn=NX
```

- `apps=NCWC` — "Nikon Camera Wireless Control" (internal SDK name for NX Field camera interface)
- `grpn=NX` — NX Field group identifier
- `pid=450` — product ID (Z9 = 0x0450 in Nikon's PTP device ID space; needs verification)
- `guid` — camera GUID, stable per device
- `seq` — session/sequence token, changes per session

Service hostname format: `<seq>-<ip-dashes>.local.` e.g. `20d4c27d-192-168-008-102.local.`

## Capture infrastructure

**Router**: GL.iNet Beryl AX (MT3000), OpenWrt 21.02-SNAPSHOT

**Problem**: MediaTek MT7981 chipset does in-driver hardware bridging for WiFi
client-to-client traffic. Even with HNAT disabled
(`echo 0 > /sys/kernel/debug/hnat/hook_toggle`), tcpdump on `br-lan` cannot
see camera↔phone traffic because it never enters the Linux kernel.

**What works**:
- `br_netfilter` is built into the kernel (`/proc/sys/net/bridge/bridge-nf-call-iptables`)
- iPhone→internet traffic is visible (must route through CPU)
- Multicast/mDNS is visible (CPU processes for all-client delivery)
- Camera mDNS becomes visible once ARP poisoning forces traffic through Mac

**Working capture method**: ARP MITM from the Mac via bettercap.

### Setup
```bash
# 1. Enable IP forwarding on Mac
sudo sysctl -w net.inet.ip.forwarding=1

# 2. Start capture
sudo tcpdump -i en0 -s 0 -w ~/Desktop/ptpip.pcap 'host 192.168.8.101 or host 192.168.8.102'

# 3. Start MITM (in separate terminal)
sudo bettercap -iface en0 -eval "set arp.spoof.targets 192.168.8.101,192.168.8.102,192.168.8.183; set arp.spoof.fullduplex true; arp.spoof on"
```

**Critical**: disconnect NX Field from cameras BEFORE starting bettercap, then
reconnect. If bettercap starts while a session is already live, it disrupts the
existing connection and NX Field can't re-establish through the Mac cleanly.

### Teardown
```bash
# Stop bettercap cleanly (sends gratuitous ARPs to restore real MACs)
# Type 'exit' in bettercap console, or:
sudo bettercap -iface en0 -eval "set arp.spoof.targets 192.168.8.101,192.168.8.102,192.168.8.183; arp.spoof on; sleep 3; arp.spoof off; quit"

sudo sysctl -w net.inet.ip.forwarding=0
```

## Next session goals

1. **Capture the full PTP/IP session**: disconnect NX Field, start MITM, reconnect,
   fire camera + change settings, stop cleanly.

2. **Wireshark analysis**: open `ptpip.pcap` with filter `ptpip` to decode
   PTP/IP frames. Key ops to look for:
   - `0x1016` SetDevicePropValue (NX Field writing a setting)
   - `0x1014` GetDevicePropDesc
   - `0x9xxx` vendor-only operations
   - First uint32 in SetDevicePropValue payload = property code

3. **Z6III**: did not advertise mDNS during the session (was holding active
   connection). Expect same TXT record format; `pid` will differ from Z9's `0x450`.

4. **Strings pass on NX Field app** (deferred):
   ```bash
   strings "/Applications/NX Field.app/Contents/MacOS/NX Field" \
     | grep -E 'kNk|MAID|0x[Dd][0-9a-fA-F]{3}|Prop|Capability|NCWC|grpn' \
     | sort -u
   ```
   (NX Field may be iOS-only; check for a Mac Catalyst build)
