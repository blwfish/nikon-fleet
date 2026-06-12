# RaceMonitor API — Reverse Engineering Notes (2026-06-12)

## Method

Captured via mitmproxy MITM proxy on Mac (192.168.8.141), iPhone proxied through
port 8080, mitmproxy CA cert installed and trusted on iOS. No certificate pinning.

## Base URL

```
https://api.race-monitor.com/v2/
```

Served behind Cloudflare (172.66.40.237). HTTP/1.1 only (no HTTP/2). Responses
Brotli-compressed, JSON body.

## Authentication

Every request includes a static app-wide API token in the POST body:

```
apiToken: a99c07b4-0759-44fc-be5d-a859eec4c96a
```

No per-user authentication required for read access. Token is hardcoded in the
RaceMonitor iOS app (v2.5.16, User-Agent: `RaceMonitor/2.5.16 (iPhone; iOS 26.5; Scale/3.00)`).

## Request format

All endpoints use:
- Method: `POST`
- Content-Type: `application/x-www-form-urlencoded; charset=utf-8`
- Parameters in request body (URL-encoded)

## Endpoints observed

### Race

#### `POST /v2/Race/RaceInfo`
Race metadata (name, date, venue, etc).

#### `POST /v2/Race/Media`
Media assets for a race (logos, images). Response: 165b — minimal content.

### Info

#### `POST /v2/Info/SubscriptionRaceList`
List of races the user has subscribed/bookmarked.

#### `POST /v2/Info/AppSections`
App navigation structure.

### Results

#### `POST /v2/Results/SessionDetails`

Request body:
```
apiToken=a99c07b4-0759-44fc-be5d-a859eec4c96a&sessionID=8660379
```

Response:
```json
{
  "Successful": true,
  "Session": {
    "ID": 8660379,
    "RaceID": 166119,
    "Name": "Practice 1",
    "SessionDate": null,
    "SessionTime": null,
    "SortMode": "qualifying",
    "SortedCompetitors": [
      {
        "ID": 75921003,
        "SessionID": 8660379,
        "RaceID": 166119,
        "FirstName": "RICH",
        "LastName": "BELLO",
        "Position": "23",
        "Laps": "9",
        "LastLapTime": "00:02:08.607",
        "BestPosition": "1",
        "BestLap": "7",
        "BestLapTime": "00:00:58.549",
        "TotalTime": "00:10:46.501",
        "Number": "91",
        "Transponder": "1530252",
        "Nationality": "BLUE",
        "AdditionalData": "V 74 RSR",
        "Category": "4",
        "LapTimes": []
      }
      // ... more competitors
    ],
    "Categories": {
      "1": { "ID": "1", "Name": "911CUP" },
      "2": { "ID": "2", "Name": "VU" },
      "3": { "ID": "3", "Name": "VO" },
      "4": { "ID": "4", "Name": "VO+" },
      "5": { "ID": "5", "Name": "VGTO" },
      "6": { "ID": "6", "Name": "GT4" },
      "7": { "ID": "7", "Name": "VGTX" },
      "8": { "ID": "8", "Name": "E" },
      "9": { "ID": "9", "Name": "VGTU" }
    },
    "CategoryString": "911CUP / E / GT4 / VGTO / VGTU / VGTX / VO / VO+ / VU",
    "ResultsProcessorVersion": 4,
    "SessionStartDateEpoc": 1780672537
  }
}
```

**Competitor fields:**
- `Position` — current running position
- `BestPosition` — best qualifying/timing position
- `BestLap` / `BestLapTime` — lap number and time of best lap
- `LastLapTime` — most recent lap time
- `TotalTime` — cumulative session time
- `Number` — car number
- `Transponder` — transponder ID (AMB/MyLaps hardware)
- `Nationality` — actually car livery color (e.g. "BLUE", "WHITE/RED", "PURPLE/GREEN")
- `AdditionalData` — car description (e.g. "V 74 RSR", "84 911 CUP")
- `Category` — numeric key into `Categories` map
- `LapTimes` — array, empty in non-live sessions; presumably populated during live timing

#### `POST /v2/Results/GroupedSessionsForRace`
Returns sessions grouped by type for a given race. Likely takes `raceID` parameter.

#### `POST /v2/Results/WatchedResults`
Results for watched/favorited competitors.

## Second hostname: cluster1.race-monitor.com

Observed in TLS SNI during passive capture but no connections appeared in mitmproxy
during the session (no live races were running). Likely a WebSocket or long-poll
endpoint for real-time lap updates, only connected when a session is actively running.
The `LapTimes: []` in `SessionDetails` responses may be populated via this channel
during live sessions.

## Image CDN

Logos served from `http://i.race-monitor.com/` (plain HTTP, redirects to HTTPS).
Example: `http://i.race-monitor.com/logos/2022_lemons_logo.png`

## Notes

- Session IDs and Race IDs are plain integers, likely sequential
- `SessionStartDateEpoc` is a Unix timestamp for session start
- No rate limiting observed during testing
- No per-user auth means any `sessionID` is queryable with the static token
