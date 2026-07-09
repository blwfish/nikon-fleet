#!/usr/bin/env bash
#
# Turns a stock Raspberry Pi OS Lite (Bookworm, 64-bit) install into an
# isolated 5GHz WiFi access point for PTP/IP capture: the phone (NX Field)
# and camera both associate directly to the Pi, so tcpdump on wlan0 sees
# ALL client<->client traffic natively. No ARP-spoofing MITM needed.
#
# This replaces the Beryl AX / bettercap workaround in
# docs/ptpip-capture-notes.md, which was required because the Beryl's
# MediaTek MT7981 does in-driver hardware bridging that bypasses the Linux
# kernel (and therefore tcpdump) for client<->client traffic. The Pi's
# brcmfmac driver has no such hardware fastpath in AP mode: every frame
# goes through mac80211/the kernel, so it's visible to tcpdump for free.
#
# Run this ON THE PI over SSH (management access via Ethernet), as root
# or with sudo. Idempotent-ish: re-running overwrites config files and
# restarts services, but won't duplicate them.
#
# Usage: sudo ./rpi-ap-setup.sh

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run as root: sudo $0" >&2
  exit 1
fi

# ---- Config — edit before running -----------------------------------------

AP_SSID="${AP_SSID:-nikon-snoop}"
AP_PASSPHRASE="${AP_PASSPHRASE:-changeme-8chars-min}"
AP_IFACE="${AP_IFACE:-wlan0}"
AP_IP="${AP_IP:-192.168.66.1}"
AP_SUBNET="${AP_SUBNET:-192.168.66.0/24}"
AP_DHCP_RANGE_START="${AP_DHCP_RANGE_START:-192.168.66.10}"
AP_DHCP_RANGE_END="${AP_DHCP_RANGE_END:-192.168.66.100}"

# 5GHz, non-DFS channel by default (36/40/44/48 = UNII-1, no radar/CAC
# delay). Cameras + phone were captured on 5GHz previously — see
# docs/ptpip-capture-notes.md. Must match your regulatory domain.
AP_COUNTRY_CODE="${AP_COUNTRY_CODE:-US}"
AP_CHANNEL="${AP_CHANNEL:-36}"
AP_HW_MODE="${AP_HW_MODE:-a}"   # a = 5GHz, g = 2.4GHz fallback

CAPTURE_DIR="${CAPTURE_DIR:-/home/pi/captures}"

# -----------------------------------------------------------------------------

if [[ "$AP_PASSPHRASE" == "changeme-8chars-min" ]]; then
  echo "ERROR: set AP_PASSPHRASE (env var or edit this script) before running." >&2
  exit 1
fi

echo "== Installing packages =="
apt-get update
apt-get install -y hostapd dnsmasq tcpdump tshark

echo "== Unmanaging $AP_IFACE from NetworkManager =="
install -d /etc/NetworkManager/conf.d
cat > /etc/NetworkManager/conf.d/unmanaged-ap-iface.conf <<EOF
[keyfile]
unmanaged-devices=interface-name:${AP_IFACE}
EOF
systemctl reload NetworkManager || true

echo "== Static IP unit for $AP_IFACE =="
cat > /etc/systemd/system/ap-iface-static-ip.service <<EOF
[Unit]
Description=Static IP for ${AP_IFACE} (capture AP)
After=network-pre.target NetworkManager.service
Before=hostapd.service dnsmasq.service
Wants=network-pre.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/sbin/ip addr flush dev ${AP_IFACE}
ExecStart=/sbin/ip addr add ${AP_IP}/24 dev ${AP_IFACE}
ExecStart=/sbin/ip link set ${AP_IFACE} up

[Install]
WantedBy=multi-user.target
EOF

echo "== hostapd config =="
install -d /etc/hostapd
cat > /etc/hostapd/hostapd.conf <<EOF
interface=${AP_IFACE}
driver=nl80211
ssid=${AP_SSID}
hw_mode=${AP_HW_MODE}
channel=${AP_CHANNEL}
country_code=${AP_COUNTRY_CODE}
ieee80211d=1
ieee80211n=1
ieee80211ac=1
wmm_enabled=1
auth_algs=1
wpa=2
wpa_passphrase=${AP_PASSPHRASE}
wpa_key_mgmt=WPA-PSK
rsn_pairwise=CCMP
# No client isolation: camera and phone need to see each other.
ap_isolate=0
EOF
set_default_var() {
  # Idempotent set-or-append of KEY="VALUE" in a /etc/default/* style file.
  # A plain sed replace assumes the line already exists uncommented in the
  # expected form, which isn't guaranteed across hostapd package versions.
  local file="$1" key="$2" value="$3"
  touch "$file"
  if grep -q "^${key}=" "$file"; then
    sed -i "s|^${key}=.*|${key}=\"${value}\"|" "$file"
  else
    echo "${key}=\"${value}\"" >> "$file"
  fi
}
set_default_var /etc/default/hostapd DAEMON_CONF "/etc/hostapd/hostapd.conf"
set_default_var /etc/default/hostapd DAEMON_OPTS ""

echo "== dnsmasq config (DHCP only, isolated network, no upstream DNS) =="
install -d /etc/dnsmasq.d
cat > /etc/dnsmasq.d/ap-iface.conf <<EOF
interface=${AP_IFACE}
bind-interfaces
except-interface=lo
dhcp-range=${AP_DHCP_RANGE_START},${AP_DHCP_RANGE_END},255.255.255.0,12h
domain-needed
bogus-priv
no-resolv
no-poll
EOF

echo "== Capture dir =="
install -d -o pi -g pi "$CAPTURE_DIR"

echo "== Enabling services =="
systemctl unmask hostapd
systemctl daemon-reload
systemctl enable ap-iface-static-ip.service hostapd dnsmasq
systemctl restart ap-iface-static-ip.service
systemctl restart hostapd
systemctl restart dnsmasq

echo
echo "Done. SSID '${AP_SSID}' on channel ${AP_CHANNEL} (${AP_HW_MODE}), AP at ${AP_IP}."
echo "Verify with:  iw dev ${AP_IFACE} info   (should show type AP)"
echo "              journalctl -u hostapd -n 50"
echo
echo "Capture a session with, e.g.:"
echo "  sudo tcpdump -i ${AP_IFACE} -s0 -w ${CAPTURE_DIR}/session-\$(date +%Y%m%d-%H%M%S).pcap"
echo
echo "This network (${AP_SUBNET}) has no internet uplink by design — NX Field"
echo "may prompt to 'stay connected' with no internet; accept it."
