# RaceMonitor Live-Timing Capture Session — 2026-07-18

Session ran entirely on the studio (`blws-M4-Max-Studio.local`, `/Volumes/Files/claude/nikon-fleet`),
via the Claude Browser pane — no MacBook, no Playwright, no MQTT/SQLite this time. Goal: catch flag-state
*diversity* (especially Red/Black, never observed on 7/10) across several series in one day, to inform the
per-series flag→action config noted in [todo.md](todo.md).

Builds on [racemonitor-live-timing-session-2026-07-10.md](racemonitor-live-timing-session-2026-07-10.md)
(PCA at Watkins Glen, Process A/B) and [racemonitor-api-notes.md](racemonitor-api-notes.md) (authenticated
REST API). This session used only the public `race-monitor.com/Live/Race/<id>` widget, read directly
through the browser pane — chosen deliberately over the MacBook's more elaborate mitmproxy setup, since
today's goal was live human-watched observation, not raw WebSocket capture.

## Races watched

| Event | Race ID | Venue |
|---|---|---|
| NASCAR Canada Series | 163957 | Calabogie |
| Trans Am | 162827 | Watkins Glen |
| 2026 FRP (Formula/club support races) | 167750 | Watkins Glen |
| 2026 International GT | 167749 | Watkins Glen |

Trans Am is the televised headline event at the Glen; FRP and International GT are support races run
around it. Confirmed live: the headline race's schedule held closely, while the support races' schedule
moved around a lot — reinforced directly when a lightning delay held FRP/International GT for well over an
hour while Trans Am ran on schedule.

## Pre-race findings

- **No forward session schedule was available for any of the four races**, via any path. The circular
  `(i)` info button next to "Live Timing" (native-app chrome, confirmed absent from both desktop and
  mobile-width renders of the web widget) was empty for all four — normally populated for PCA events per
  prior experience, but not this time, for any series.
- **Event-level (not session-level) gate-open/close windows are embedded directly in the `/Live` and
  `/Live/Section/<n>` listing pages' HTML**, as inline JS calls:
  ```
  addCurrentDate(163957, 1784376000, 1784519100)   // Unix start/end
  ```
  This is the whole-weekend gate window (e.g. "7/18 7:00 AM – 7/19 10:45 PM"), not a session-by-session
  schedule — confirms there's no reliable forward schedule source at all for a day like this one; the only
  option was watch-and-react.

## New vocabulary/UI finding: weather-hold pseudo-entry

A rain/lightning delay is signaled by RaceMonitor as a **synthetic pseudo-competitor row injected into the
leaderboard itself** — `#LD "Lightning Delay"`, class `"SYS"` — not via the flag-state field. First seen on
FRP, held for well over an hour. Session-interruption status can apparently ride in more than one field
depending on cause; a consumer watching only the flag string would miss a weather hold entirely.

## Flag-state findings

**Directly observed and confirmed this session:** `Green`, `Yellow`, `Red`, `Finish` — all witnessed with
specific before/after transitions, not inferred.

**Not confirmed, despite one internal session summary claiming otherwise:** a `Black` flag string. One
end-of-day recap in this session asserted "Black" as confirmed DOM vocabulary alongside the others, but
tracing every flag observation in the transcript turns up no actual Black-flag sighting anywhere in the
day's four races — that claim appears to be an error in the recap (produced partway through a mid-session
model switch to Haiku), not a real finding. Treat Black as **still unconfirmed**, same as 7/10.

### Diversity actually captured

| Race / session | Flag sequence | Character |
|---|---|---|
| NASCAR Canada, Practice | (already running) → **Red** → Green | Long hold, roughly an hour — by far the longest stoppage of the day |
| NASCAR Canada, Qualifying (Group 1) | never confirmed green | **Anomaly** — full 10-minute session window elapsed with zero lap times recorded for any driver; no weather indicator shown for this one |
| Trans Am, Race 1 | Green → Yellow → Green → **Finish** | One caution, roughly 15–20 minutes; #16 Matthew Brabham won (30 laps, best lap 1:44.939) |
| Trans Am, TA2 Race 1 | Green → Yellow (long) → Green → Yellow (brief) → Green | Two cautions of visibly different character — one extended, one short and mild |
| FRP, Atlantic/FE-2/F2000 Heat Race 1 | Green → **Finish** | No caution; #06 Harbir Dass won |
| FRP, F1600 Heat Race 1 | Green → **Finish** | No caution; photo finish, #17 Cooper Travis over #06 David Ybarra by 0.116s |
| International GT, Race 3 | Green → **Finish** | No caution; #7 Charles Wicht won |

Durations above are qualitative on purpose — the live narration's own running "elapsed minutes" callouts
didn't fully reconcile with message timestamps in at least one case (TA2's second-observed yellow), so
exact minute counts from that narration shouldn't be trusted as precise data.

## New technical findings (beyond 7/10)

- **Red freezes the session `Timing` clock; Yellow does not.** Confirmed directly: NASCAR Canada's clock
  sat frozen at `00:43:01` for the entire ~hour-long red, then resumed counting from that exact value the
  moment it cleared. Trans Am's yellows, by contrast, showed the `Timing` clock advancing normally
  throughout (only lap times ballooned).
- **Per-car and session-level timers don't necessarily freeze together.** Immediately after NASCAR
  Canada's red cleared, one competitor's `Last Time` read over an hour — the per-car lap timer apparently
  kept running through the red flag even though the session clock was frozen. Worth folding into the
  Gateway design: don't assume a frozen session clock implies all car-level fields are frozen too.
- **A purple highlight on a competitor row appears to mark "most recent fastest lap."** Seen consistently
  on both NASCAR Canada practice and FRP qualifying, moving to whichever driver had just improved their
  best time.
- **"No current race or not receiving data" is a distinct state from the stale-idle state seen on 7/10**
  (where the widget kept showing the last completed session's leaderboard). Both appear to represent
  "nothing live right now," inconsistently, depending on circumstance — worth treating as two states to
  detect rather than one.
- **`Time to go` can be reliably pre-populated once a grid has formed** (e.g. FRP showed `Time to go:
  01:15:00` while queued, well before green) — this contrasts with 7/10's finding that `Time to go` was
  unreliable pre-green. Reliability may depend on session phase/type rather than being uniformly bad;
  worth re-testing before assuming either way.
- **`Laps to go` sentinel `9999` also appears during genuinely-running time-based sessions**, not just
  pre-session idle (seen on NASCAR's stalled qualifying and mid-race on International GT while actively
  green) — broadens the known cases where `9999` shows up beyond the "no lap cap" case from 7/10.

## Open items for next session

- [ ] Black flag still unconfirmed — no series watched so far has thrown one on camera; keep watching
- [ ] IndyCar not covered by any race watched this session or on 7/10 — zero data on IndyCar black-flag
      semantics (single-car penalty vs field event)
- [ ] Process B (raw WebSocket capture) untouched this session — still no captured wire schema for
      `cluster1.race-monitor.com`
- [ ] No structured/replayable data file was produced — this entire day's observations exist only as
      narrated chat state in this session's transcript (claude-journal conversation
      `f932a152-6611-43de-a12a-a5a927c15bf2`), not as a file or database. If a structured timeline is
      needed later (e.g. as a Simulator replay fixture), it has to be extracted from that transcript.
- [ ] Investigate the NASCAR qualifying zero-lap-data anomaly — full session window elapsed with no timing
      data recorded for any driver, no weather indicator shown; unclear if this is a RaceMonitor feed gap
      or something about that specific session
