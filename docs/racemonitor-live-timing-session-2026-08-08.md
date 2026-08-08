# RaceMonitor Live-Timing Capture Session — 2026-08-08

PCA **Schattenbaum Showdown** club race weekend, **NJMP Thunderbolt**, race ID **168304**. Saturday only.

Unlike [2026-07-10](racemonitor-live-timing-session-2026-07-10.md) (human-watched, ad-hoc) and
[2026-07-18](racemonitor-live-timing-session-2026-07-18.md) (human-watched, multi-series), this session ran
**unattended** as a 5-minute cron task from 09:00 to 17:20 EDT — 83 fires, 9 sessions captured end-to-end,
no human in the loop. Raw data:

- [racemonitor-capture-2026-08-08.jsonl](racemonitor-capture-2026-08-08.jsonl) — 89 append-only poll records
- [racemonitor-capture-2026-08-08-state.json](racemonitor-capture-2026-08-08-state.json) — 117-key running findings file, ~537 KB

The unattended format is what made the day valuable. A human watching would have gotten the flag sequence;
what the cron got instead was **repeated cold-client reproduction** of every structural claim (13 independent
cold starts of the idle signature alone), which is the difference between "I saw it work" and "this selector
is safe to build a camera controller on."

Feeds the per-series flag-interpretation config in [todo.md](todo.md).

---

## Two sources, and only one of them is live

| | Timing widget (`api.race-monitor.com/Timing?raceid=…`) | REST results API (`/v2/Results/…`) |
|---|---|---|
| Transport | React SPA, needs a rendered browser | POST form-urlencoded, static apiToken |
| Live flag state | **Authoritative** | Absent entirely |
| Session boundaries | **Authoritative** | Lagged, frozen, or wrong |
| Final classification | Correct, and arrives first | Correct eventually — up to 111 min later |
| Entry metadata | Sometimes lossy | Sometimes lossy, *differently* |

**The single most important operational conclusion of the day: the REST API cannot drive camera control, and
"the API looks frozen" is not a session-ended signal.** This was not a suspicion carried in from prior
sessions — it was established the hard way, three separate times, and each mechanism is documented below.

---

## Chronological session log

Nine sessions, three run groups (Blue / Yellow / Red) rotating through Warm-Up, Sprint 1, Sprint 2.

| # | Session | Cars | Green (derived) | Checkered | Teardown | Posted | Δ |
|---|---|---|---|---|---|---|---|
| 1 | Blue Warm-Up | 14 | 09:30:08 | ≤09:40:22 | — | 09:25a | +5m |
| 2 | Yellow Warm-Up | 19 | 09:44:42 | 09:48:52–09:53:57 | — | 09:40a | +5m |
| 3 | Red Warm-Up | 9 | 09:54:36 | 10:04:24–10:08:56 | ~10:14:01 | 09:55a | −0.4m |
| 4 | Blue Sprint 1 | 21 | 10:39:22 | 11:10:28 | ~11:14:34 | 10:35a | +4m |
| 5 | Yellow Sprint 1 | 29 | 11:19:40 | 11:51:05 | 11:57:37 | 11:15a | +5m |
| 6 | Red Sprint 1 | 24→19 | 13:08:07 | 13:39:41 | 13:46:30 | 01:05p | +3m |
| 7 | Blue Sprint 2 | 21→20 | 14:14:41 | 14:40:57 † | 15:19:38 | 02:10p | +5m |
| 8 | Yellow Sprint 2 | 29→20 | 15:42:26 | 16:13:06 | ≤16:19:41 | 02:50p | **+52m** |
| 9 | Red Sprint 2 | 24→22 | 16:22:58 | 16:52:57–16:54:47 | **16:59:49** | 04:35p | **−12m** |

† terminated early, under caution, with no restart and no white flag.

Green instants are derived as `wall_clock − session_clock`, not observed. That derivation was checked ~20
times within single sessions and **never drifted by even one second** — including across a full-course
caution and a restart. It is the cheapest reliable way to timestamp a green after the fact.

### The schedule is not a schedule

Posted-vs-actual offsets in order: on time, +5, +15, +30, +40, **+52, −12**. The error **changed sign** by
~64 minutes across one session boundary — Yellow Sprint 2 ran nearly an hour late, then Red Sprint 2 went
green *twelve minutes early* because race control simply ran it as soon as the track cleared into a 1h45m
posted gap.

> **Rule:** posted times are a not-before floor for the first session of a block and an ordering hint
> thereafter. Never extrapolate a running lateness offset. A controller that used the posted 04:35p to decide
> when to start paying attention would have **missed the start of the last race of the day.** Poll
> unconditionally, all day, and key off the widget's own ARMED signal.

---

## The flag carrier — the headline structural result

The flag is **the sole `font-weight: bold`/`700` child of `.timingHeader`, with no class and no id.**

```
top: 7px; right: 40px; font-weight: bold; text-align: right; color: rgb(0, 255, 0);
```

Both the discriminator (`font-weight: bold`) and the value (`color: rgb(...)`) live in the raw inline
`style` attribute, so no `getComputedStyle` call is needed. Read **both** the text and the color.

| State | Text | Color |
|---|---|---|
| Green | `Green` | `rgb(0, 255, 0)` |
| Yellow | `Yellow` | `rgb(255, 255, 0)` |
| Finish | `Finish` | `rgb(255, 255, 255)` |
| Idle / Armed | `""` (empty) | `rgb(255, 255, 255)` — **retained from the prior state** |

This signature was validated across **6+ live transitions and ~30 cold-client reproductions**, held at child
index 6 of 13 every single time, and — the strongest evidence — **generalised to Yellow with zero extractor
changes** when the day's first caution appeared at poll 64, a state it had never seen. Only the text and the
color hex differed.

Two consequences worth stating explicitly:

- **Color alone is not a valid flag signal.** Idle and Finish are both white; only the text distinguishes
  them. Reproduced 6 times. An extractor must treat empty text as IDLE explicitly.
- **Do not grep body text for color words.** Livery strings in the leaderboard (`GREEN/BLACK`,
  `WHITE/RED/BLACK`, `YELLOW/GRAY`) produce a large, non-constant false-positive floor. This was proven
  wrong multiple times before the structural signature was found, and it is the single easiest mistake to
  make when building this.

Still never observed, after two race weekends: **Red**, **Black**, **White**, **Meatball**, and a checkered
distinct from `Finish`.

---

## The state machine

```
IDLE ──► ARMED ──► GREEN ──►┬─► [WHITE/FINAL LAP] ──► FINISH ──► (post-checkered hold) ──► IDLE
                            │        ▲                   ▲  │
                            └► YELLOW┘                   └──┘  Finish ⇄ Yellow can oscillate
                                  └──► GREEN (restart)
```

| State | Flag text | lapsToGo | Clock | timeToGo |
|---|---|---|---|---|
| IDLE | `""` | **0** | `00:00:00` | `00:00:00` |
| ARMED | `""` | **9999** | `00:00:00` | duration (late-populated) |
| GREEN | `Green` | 9999 | advancing | counting down |
| FINAL LAP | `Green` | **1** | advancing | `00:00:00` |
| FINISH / hold | `Finish` | 9999 | **still advancing** | `00:00:00` |

**Never infer end-of-session from `timeToGo == 0` or `lapsToGo == 0` alone.** `ttg == 0` is 3-way ambiguous
(no duration configured / checkered / idle). `ltg == 0` means idle specifically — during a live session it is
`9999`, which resolves to `9999 = no lap limit in effect` rather than a magic number.

### ARMED is a real, distinct state

`IDLE → ARMED` is the transition a camera controller should key on: `lapsToGo 0 → 9999`, timeToGo restored,
and a fresh all-zero-times grid loaded — **while the flag text is still empty.** Arm→green latency ranged
from **≤34s to 3m28s**, with no relation to field size (Blue Sprint 1 and Blue Sprint 2 both had 21 cars,
34s vs 3m28s). One arm (Blue Sprint 2) held `ttg == 00:00:00` for ~3 minutes before the duration populated,
so **`ttg` is a late arm event and its absence does not mean "not armed."**

### The white-flag window is the only predictive signal in the feed

At time expiry, `lapsToGo` drops off the 9999 sentinel to a real integer (`1`) while the flag stays Green.
That gives **roughly one leader lap (~1m23s) of advance warning** before the checkered. Everything else in
this feed is coincident or lagging.

Caveats, both observed: it does **not** fire when a session is terminated under caution (Blue Sprint 2), and
it was **missed twice** by 5-minute polling. Catching it reliably needs sub-lap cadence once `ttg` is inside
~2 minutes.

### Finish does not latch

Blue Sprint 2 ran `Finish → Yellow → Finish → Yellow → Finish` — **two complete round trips over 32 minutes**
of post-checkered hold, all within one verified session (clock re-derived the same green instant to the
second at every probe; `lapsToGo` never reset). The flag was the *only* mutating header field across all four
edges.

> **Rule:** `Finish` is a transient track condition, not an end-of-session marker. End-of-session must key on
> the teardown signal: **`lapsToGo 9999 → 0` AND clock → `00:00:00`.** Three independent lines of evidence
> converge on this.

### The post-checkered hold is long, variable, and not quiet

| Session | Hold |
|---|---|
| Blue Sprint 1 | ~4m06s |
| Yellow Sprint 1 | 3m37s–6m32s |
| Red Sprint 1 | 2m39s–6m49s |
| **Blue Sprint 2** | **~38m41s** |
| Red Sprint 2 | 3m38s–6m52s |

The 3.5–6.5 min "band" derived from the first three sessions was exceeded **5.7×** by the fourth. That band
was a sampling artifact, not a property. Treat hold duration as unbounded.

During the hold the leaderboard **mutates, and can rewind.** Blue Sprint 2's grid was observed rendering
`Laps`/`Diff`/`Gap` describing the race at ~lap 13 while `LastTime`/`BestLap`/`BestTime` described the end of
the race — an internally inconsistent row set corresponding to **no actual instant of the session.** The
rewind then stopped *without* re-converging on the true final, and teardown froze the wrong grid.

This kills the "results frozen across consecutive polls ⇒ session over" heuristic outright — which is exactly
what this capture task originally used as its stop condition, and why the stop condition was rewritten
mid-day to the validated teardown signal.

---

## Why the REST API cannot drive live control

Three independent failure modes, each confirmed repeatedly:

**1. In-progress sessions get frozen stub records.** A session that is running gets a low-lap-count snapshot
published and then **never updated**. Red Sprint 2's two records sat frozen at **6 laps** for the entire
session and were still frozen 20 minutes after teardown — while the widget showed the correct 22-lap final.
There is **no provisional/final flag** on the record to tell them apart.

**2. Real results land later, under a *new* session ID.** Sessions publish 2–3 sibling records. Blue Sprint 2's
third sibling took **111 minutes**. Publish lag is unbounded in practice; treat it as such.

**3. `SessionStartDateEpoc` is not a publish time and duplicate epochs are common.** Sibling records share an
epoch, so "highest epoch wins" silently picks an arbitrary one. Selection must group on `(Name,
CategoryString)`, and even then no rule found today reliably identifies the final record from the API alone.

At one point the API's newest session and the session the widget was *displaying* were **different sessions** —
an API-driven consumer at that instant would have reported the wrong run group, wrong field, and wrong car
numbers, with `Successful: true` and no error.

### API schema traps (all hit live, all silent)

Every one of these returns `null`/`0`/empty rather than an error:

| Trap | Correct form |
|---|---|
| Wrong path prefix → 302 → **200 + text/html** | assert `Content-Type: application/json` **and** `.Successful` |
| `.Groups` | `.GroupedSessions` |
| `.Session.Competitors` | `.Session.SortedCompetitors` |
| `.SessionStartDateEpoch` | `.SessionStartDateEpoc` *(truncated spelling, in the API)* |
| `.Categories[7]` (array index) | `.Categories["7"]` **(dict keyed by string ID)** |
| Numeric fields | **all strings**, and blank ≠ zero |
| `BestLap` | a **lap number**, not a time — the time is `BestLapTime` |
| `LapTimes` | **always `[]`**, on every record, all day |

The API-path traps were re-walked from scratch on roughly **half of all cold fires** across the day. Recovery
is per-trap, not per-family: knowing about one does not prevent the next. That recurrence rate is itself the
finding.

> **Rule:** do not ship this as ad-hoc curl/jq. Write one checked-in helper per endpoint that hardcodes the
> verified path and field names and asserts `Content-Type` + `.Successful` + expected-key-present-and-non-empty
> before returning. Per the CLAUDE.md syntactic-semantic seam rule, the schema needs a single source of truth
> rather than being retyped from memory at each call site.

---

## Widget DOM extraction

### Rows

```js
document.querySelectorAll('.racerRowStacked')   // correct
document.querySelectorAll('[class*=racerRow]')  // 522 for a 29-car field (~18× over-count)
document.querySelectorAll('.racerRow')          // 0 — no element is ever classed bare racerRow
document.querySelectorAll('[class*=timingRow]') // 0 — plausible-sounding, entirely fictional
```

Three ways to get it wrong, failing in **opposite directions**, none of which raises an error. The leaderboard
is div-based, not tabular — `<tr>` count is always 0.

> **Rule:** always cross-check the row count against an independent witness (the `.position` node count) before
> trusting it. **Never accept zero rows as "the session is empty"** — per the No-Log-Scraping rule, collapsing
> a selector miss into "not running" is exactly the inference that must not be made.

### Cells

Ten value cells resolve reliably per row (`.lapsValue`, `.lastTimeValue`, `.bestLapValue`, `.bestTimeValue`,
`.diffValue`, `.gapValue`, plus `.position`, `.racerName`, `.racerCategory`, `.additionalData`). Blanks are
always empty *text*, never a missing node. Note the two-token classNames (`diffValue alignRight`) — use
single-class selectors, never exact `className` string matches.

**There is no `.number` cell and no `.name` cell.** The car number is fused into `.racerName` as
`"#17 MARK CALZARETTA"`; the class token is a sibling span. Parse with `/^#(\S+)\s+(.*)$/` and **keep the
number as a string** — observed values include `01`, `029`, `8`, `333`, so integer parsing silently destroys
leading zeros and collides `01` with `1`.

### `Diff` and `Gap` are not what they look like

`Diff` is a **per-lap-block accumulator**, not time behind the leader. Both columns **flip type mid-grid**
between a time (`43.158`) and a lap count (`+1 lap`) — with inconsistent pluralization (`+1 lap` in Diff,
`+1 laps` in Gap). Both can be **negative**. Both can be blank for an entire lap block.

`BestTime` is a **prefix minimum, not a fact**: the API stub for Red Sprint 2 reported the leader's best as
`01:19.966`, against the true final `01:19.690` — 0.276s slow. Both sources are upper bounds until teardown.

### Field size shrinks mid-session, and both sources see it differently

Entry counts are arm-time rosters, not starters. Yellow Sprint 2 armed **29** and started **20** — class `I`
disappeared entirely because both its entries were non-starters. **Run-group identity must come from the
API's session-level `CategoryString`, not from a census of the widget's rendered class tokens**, for reasons
that became very concrete on the last session of the day.

---

## New this fire (poll 83): neither source is a superset

Discovered while confirming the frozen Red Sprint 2 final grid during cooldown.

**The trailing carriage return is a live-feed artifact.** The widget renders a trailing `\r` on 21/22
`.racerCategory` and 20/22 `.additionalData` cells. **Zero** of the 22 corresponding API strings contain one.
The CR is introduced by the live path, not the source data as the results API serves it — so prior findings
about "which fields carry a CR" are scoped to the widget only, and **CR presence must never be used as a
checksum, discriminator, or source identifier.**

**The blank class cell is provably GTC5.** `#51 HARGESHEIMER` renders an empty `.racerCategory`. The API gives
`Category: "7"` → `Categories["7"].Name == "GTC5"`. He was the field's **only** GTC5 entrant, so losing that
one cell erases the entire token from any widget-derived class census. Independently corroborated by set
difference against the session `CategoryString` (10 tokens vs the widget's 9 + one blank; the difference is
exactly `{GTC5}`). And per poll 81, the widget rendered `GTC5` for this car at arm time (16:22:40) and blank
by 16:41:39 — **a mid-session mutation, not a static gap.**

**Loss runs in opposite directions on different rows:**

| Car | Widget | API |
|---|---|---|
| `#51` | category `""`, additionalData `"WHITE"` | `GTC5`, `"WHITE"` + `"11 997.2 CUP"` |
| `#201` | `"WHITE / 19 718 CAYMAN CLUBSPORT"` | `"WHITE"` + `"19 7"` *(truncated)* |

Otherwise the widget's `.additionalData` is exactly `Nationality + " / " + AdditionalData` — and note
`Nationality` is a misnomer carrying the **livery string**, which is precisely the source of the color-word
false positives that make body-text flag grepping useless.

> **Rule:** merge entry metadata across both sources, keyed on car number **as a string**, taking the non-empty
> value from either side and flagging disagreements. Never fill a missing field with `""` or a default —
> carry it as explicitly unknown. A blank class must mean *unknown*, never *no class*.

This is the Parallel Implementation Rule applied to two data *sources* rather than two code implementations:
the widget and the API look substitutable, and every behavioral property their shared shape doesn't capture —
which fields can blank, which can truncate, whether strings carry a CR — diverges silently. **Severity for
camera control is medium-high**: any "shoot cars in class X" rule reads exactly these fields, and both sources
will hand back a confident wrong-or-empty answer for at least one car per session. A single-entrant class is
the worst case, because the rule simply never fires and nothing errors.

---

## Recommended detector, as validated

```
poll the widget unconditionally, all race day, ~5s cadence when armed or green

flag      = sole bold/700 child of .timingHeader, no class, no id
            → read BOTH textContent and the color from the inline style attr
            → empty text = IDLE or ARMED (disambiguate with lapsToGo), never "white = Finish"

ARMED     ← lapsToGo 0 → 9999                     (start of interest; ttg may lag ~3 min)
GREEN     ← flag text "Green"
PRE-END   ← lapsToGo leaves 9999 for a small int  (~1 lap of warning; absent on caution-terminated)
CAUTION   ← flag text "Yellow"                    (can also occur DURING the hold)
RESTART   ← Yellow → Green                        (coincident only — zero lead time available)
END       ← lapsToGo 9999 → 0 AND clock → 00:00:00   ← the ONLY reliable done signal

never trigger on: ttg == 0, lapsToGo == 0 alone, flag color alone,
                  "Finish" as terminal, frozen results, frozen API, posted schedule times
```

---

## Open items for next time

**Flag vocabulary.** Red, Black, White, Meatball, and a checkered distinct from `Finish` remain unobserved
across two weekends. Also unknown whether local vs full-course yellows render differently — both of today's
cautions were read only through the carrier.

**Restart resolution.** The `Yellow → Green` restart was bracketed to 58s with no intermediate state visible.
Whether one exists needs a fire probing at ~1s during a caution. A restart is a high-value shot moment and
the feed gives **zero** lead time, so a controller must either hold finish-posture through every caution or
accept reacting a poll late.

**White-flag capture.** Missed twice at 5-minute cadence. Needs sub-lap polling once `ttg` is inside ~2 min.

**Hold-state oscillation.** Blue Sprint 2's `Finish ⇄ Yellow` cycling and its leaderboard rewind were observed
but not explained. Whether the rewind is a replay, a resync, or a results-processor artifact is unknown.

**`#201`'s API truncation** (`"19 7"`) is unexplained — plausibly the timing operator mid-edit when the stub
froze, but not established.

**Widget live transport.** Never inspected. All widget findings this session came from rendered DOM only.
A raw WebSocket capture (attempted but not completed on 7/10) would settle where the CR is introduced and
whether the rewind is visible on the wire.

**Sunday 8/9** was not captured — this task was Saturday-only.

---

## Method note

The state file caught **several of its own wrong assumptions** during the day by checking rather than
assuming: the API lag model (revised three times, ending at "unbounded"), row-count-as-field-size, text-grep
for flags, the hold-duration band, "arm→green scales with field size," and — in this final fire — an open
question logged as unresolved that a prior fire had already answered.

That self-correction rate is the reason to trust what's left. Findings recorded here were reproduced on
independent cold clients unless explicitly marked as single observations, and inferences are labelled
separately from observations throughout the state file. Where something is bracketed rather than observed,
the bracket is given.
