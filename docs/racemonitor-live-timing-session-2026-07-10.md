# RaceMonitor Live-Timing Capture Session — 2026-07-10

Session ran on the MacBook (`/Users/blw/claude/nikon-fleet`, `/Users/blw/claude/racetrack-capture/`,
`/Users/blw/claude/mqtt-logger/`) at a live PCA event at Watkins Glen ("Clash at The Glen", Race ID
166984). **None of the artifacts below have been pulled to this machine or this repo yet** — the
MQTT/SQLite capture, the poller script, and the Firefox mitmproxy profile all still live only on the
MacBook. This doc records what was learned; the raw data is a follow-up sync.

Builds on [racemonitor-api-notes.md](racemonitor-api-notes.md) (2026-06-12 mitmproxy/iOS capture of the
authenticated app API). This session used the unauthenticated public web widget instead, and focused on
live flag-state behavior rather than the REST endpoints.

## Design discussion: camera-control network

Preceding the capture itself, a design conversation about a track-side network to receive race events
(RaceMonitor, local sensors, a handheld remote) and action fleet cameras (enable/disable auto-capture,
fire shutter, query status). Decisions:

- **Laura-remote** ([blwfish/laura-remote](https://github.com/blwfish/laura-remote), currently docs-only,
  LoRa 915 MHz) is the intended backbone, not a separate MQTT/WiFi bus — it already has group addressing,
  `SHOOT`/`KA_ON`/`KA_OFF` commands, and range/reliability designed in for exactly this use case.
- Protocol needs two extensions: (1) generalize from TX-only origination to any-node pub/sub (`Src Addr`
  already acts like a publisher field, `Dst Addr`/groups already act like topics), and (2) add a
  fire-and-forget "event" packet class alongside the existing ACK'd RPC commands, since sensor/RaceMonitor
  publishes shouldn't pay the retry/tier-escalation cost a deliberate shutter-fire command needs.
- New **Gateway** role: bridges RaceMonitor (internet) onto the LoRa network. Must filter hard — raw
  lap-by-lap updates across a full field would blow LoRa's single-channel airtime budget. Scope narrowed to
  **flag events only** (~half a dozen per session, not a stream), which removes the airtime concern.
- Auto-capture control should route through a **`CAP_ON`/`CAP_OFF` extension to Laura's existing keep-alive
  mechanism** (autonomously pulsing the shutter contact on a timer, RX-side, no per-camera computer) —
  not through nikon-fleet's USB/SDK layer, which would require a tethered mini-PC per camera and defeats
  "place and leave."
- **Flag → action mapping** (confirmed against a real session below): Green → `CAP_ON`. Yellow → **no
  action** — interesting things happen under caution, keep shooting. Red/Black → `CAP_OFF`/alert, exact
  handling still open. Flag semantics are sanctioning-body-specific (a red or black flag means something
  different in NASCAR vs. PCA) — the Gateway needs a per-event/per-series flag-mapping config, not one
  hardcoded table.

## Live flag-state findings (Watkins Glen, Race 166984, Sprint 1)

Discovered a public, unauthenticated web widget: `race-monitor.com/Live/Race/<raceid>`, backed by an
iframe hitting `api.race-monitor.com/Timing?raceid=<id>` (React SPA). No login, no app, no
phone/mitmproxy needed to watch it — a plain browser tab is enough for flag-state polling, distinct from
the authenticated `apiToken` REST API in the 2026-06-12 notes.

- **Real flag vocabulary observed: `Green`, `Yellow`, `Finish`** — not "Checkered" as assumed going in.
  Rendered as plain DOM text (not baked into an image), colored (green/amber) for Green/Yellow, plain
  white/black for Finish. Any flag→action mapping must use these exact strings.
- **`Time to go` and `Laps to go` are not reliable control signals.** On this exhibition sprint (no lap
  cap, time-limited), `Laps to go` stayed pinned at sentinel `9999` for the entire session. `Time to go`
  didn't start counting down until ~5 minutes after the green flag (apparently populated by the timing
  operator after the fact, not before), then clamped to `00:00:00` at Finish rather than going negative.
  The flag-state field itself — driven by the actual timing-loop hardware, not operator config — is the
  only field worth triggering on.
- **Session rollover is not instant after Finish.** The elapsed `Timing` clock kept climbing for several
  minutes post-Finish (cool-down/in-lap) before the page would show anything else. A "stop on Finish"
  trigger fires correctly; "session has changed" does not follow immediately.
- Session name itself changes with a lag too — the page showed a stale prior-session ("Qualifying")
  leaderboard for several minutes past Sprint 1's scheduled start time before flipping over. Real event,
  running late, not a caching artifact — worth assuming a similar lag exists at every session boundary.
- **The live feed rides a WebSocket that available Chrome browser-automation tooling cannot see** (HTTP
  request/response visibility only) — and direct navigation to `api.race-monitor.com` was blocked outright
  by that tool's own domain policy. DOM polling of the rendered page worked as a full substitute for this
  session's purposes (every Green→Yellow→Green→Finish transition was caught via ~10s polling), but the
  actual WS wire format/schema is still uncaptured.

## Process A — DOM-scrape → MQTT → SQLite pipeline (built and run)

Built and validated end-to-end same day, then left running unattended:

- **Playwright poller** (`/Users/blw/claude/racetrack-capture/racemonitor_poller.py`) loads the live page,
  polls the flag/session-state DOM elements every few seconds, diffs against last-seen state, publishes a
  structured event to MQTT only on change.
  - `page.goto(..., wait_until="networkidle")` never resolves on this page (constant ad-tracker beacon
    traffic) — used `wait_until="load"` + a fixed 3s settle delay instead.
  - `.displayContainer` is not a unique selector — it's reused by loading/no-data/promo panels that
    coexist in the DOM. Used `.timingHeader` instead (unique to the live view).
- **Mosquitto** broker on `localhost:1883` (via `brew services`).
- **mqtt-logger** (from `blwfish/mqtt-logger`, SQLite backend, not MariaDB) subscribing and writing to
  `/Users/blw/claude/mqtt-logger/data/mqtt_events.db`. Needed a `from __future__ import annotations`
  compat patch to `mqtt_logger.py`/`query_events.py` in this local clone — the MacBook's only Python
  3.10+ interpreter (Homebrew python@3.12) has a broken `pyexpat`/system-`libexpat` linkage that breaks
  venv creation, so the daemon runs on system Python 3.9, which doesn't support the `str | None` syntax
  those files used. Behavior-preserving on 3.10+, but a real edit to a cloned repo — not yet decided
  whether to carry forward or fix the Python environment instead.
- Confirmed working end-to-end, including catching a real live Yellow-flag transition during the smoke
  test itself:
  ```
  racemonitor/166984/state → {"session": "Sprint 1", "flag": "Yellow", "timing": "00:16:11", ...}
  ```
- Left running detached (`nohup` + `disown`) capturing continuously through the rest of the day's
  sessions. Log at `/tmp/racemonitor_poller.log` on the MacBook; stop with `pkill -f racemonitor_poller.py`.
- **Status as of session end: presumed still captured data in `mqtt_events.db` on the MacBook, not yet
  pulled over or analyzed.** The session ended when the laptop's battery died, with no later follow-up
  session picking the analysis back up.

## Process B — mitmproxy raw WebSocket capture (attempted, not completed)

Goal: capture the actual WS wire format/schema (Process A only observes rendered DOM state, not the
underlying message shape). Run in a separate, isolated Firefox profile (own cert store, doesn't touch the
system Keychain) pointed through `mitmdump`, kept deliberately independent of Process A's own browser
session so a proxy hiccup couldn't perturb the live capture.

Blocked by a chain of friction, abandoned for the night without a working capture:

1. A leftover `mitmweb` process had been running unnoticed since the original 2026-06-12 capture session
   (a full month) — squatting port 8081, which collided with the new mitmdump proxy's chosen port. Killed;
   switched to port 8082.
2. Firefox's CA-trust import for `~/.mitmproxy/mitmproxy-ca-cert.pem` alone wasn't sufficient — Firefox has
   a separate heuristic (`security.certerrors.mitm.priming.enabled` in `about:config`) that flags known
   interception-software certificates independent of CA trust; had to be disabled explicitly.
3. Once traffic was flowing through the proxy, the site's own ad-blocker-detection script began firing
   after mitmproxy re-signed the surrounding ad-tech domains (pubmatic, doubleclick, quantcast,
   scorecardresearch, gumgum, id5-sync, etc.) — indistinguishable from a real local ad-blocker to the page,
   unrelated to whether one was actually running.
4. Even after clearing the above, the `api.race-monitor.com` iframe (the one carrying live data) never
   appeared in the capture — traced to a real ad-blocker extension actually running in that Firefox
   profile, killing the iframe outright. Session ended (battery) before confirming iframe traffic actually
   reached the capture with the blocker disabled.

**Net result: no raw WS frames were ever captured.** `www.race-monitor.com` (outer page/assets) traffic
was confirmed flowing through mitmproxy; the `api.race-monitor.com` iframe traffic was not.

## Open items for next session

- [ ] Pull `/Users/blw/claude/mqtt-logger/data/mqtt_events.db` and
      `/Users/blw/claude/racetrack-capture/racemonitor_poller.py` over from the MacBook; confirm the poller
      is still running or check how much of the day it actually captured before stopping
- [ ] Analyze the captured event stream — actual flag-transition timing/frequency across a full race day,
      not just the one session watched live
- [ ] Retry Process B (raw WS capture) with the ad blocker disabled from the start in that Firefox profile,
      or fall back to the original mobile-app + mitmproxy approach from the 2026-06-12 session (far less
      ad-tech noise)
- [ ] Decide on the `mqtt_logger.py`/`query_events.py` `from __future__ import annotations` patch: carry it
      forward in that clone, or fix the MacBook's Python 3.10+/`pyexpat` environment so it's unnecessary
- [ ] Sketch the concrete `CAP_ON`/`CAP_OFF` extension to Laura's protocol (`laura-remote` repo, not this
      one), and the pub/sub + Gateway-role generalization discussed above
- [ ] Decide Red/Black flag handling (currently "CAP_OFF/alert, TBD") and design the per-event/per-series
      flag-mapping config the Gateway role needs
