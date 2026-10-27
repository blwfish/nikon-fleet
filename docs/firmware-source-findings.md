# Firmware source: what the Nikon site actually looks like

Observed 2026-10-27 by fetching the live pages with `curl` (UA `nikon-fleet/0.1 (firmware check)`).
Fixtures of everything below live in `tests/fixtures/nikon-download-center/`.
Extends `firmware-archive-spec.md` §10 "Automatic firmware acquisition", which assumed
"no machine-readable API" — still true, but the *human* pages are static HTML and parseable.
**This doc needs fresh-session review before the download half is built** (see §5).

## 1. nikonusa.com is not where firmware lives

- `https://www.nikonusa.com/support/downloads` → redirects to `/redirect/support/downloads`,
  a Next.js page titled "Error | Nikon USA" (HTTP 404). No firmware listing is reachable there.
- Its footer links to `https://downloadcenter.nikonimglib.com/en/index.html`.
- The firmware is published by **Nikon Corporation's Download Center**
  (`downloadcenter.nikonimglib.com`), locale-prefixed (`/en/…`). nikonusa merely links to it.
  So the "pluggable source" is really "a download-center locale/site", and the first
  implementation targets `downloadcenter.nikonimglib.com/en`. A different subsidiary
  site would be another `FirmwareSource` impl (or another base URL/locale if it uses the same engine).
- `robots.txt` is a 404 (no stated crawl policy). We make one request per model per run, with a UA that names the tool.

## 2. Page structure (all server-rendered static HTML, no JS needed to *read* version info)

| Page | URL | Role |
|---|---|---|
| Product page | `/en/products/{id}/{Name}.html` — Z 9 = `589/Z_9`, Z6III = `629/Z6III`, Z 5 = `552/Z_5` | Lists the product's firmware row(s) and software rows |
| Firmware page | `/en/download/fw/{n}.html` — Z 9 = 607, Z6III = 574 | Release notes, OS picker, licence gate |
| Update feed | `/en/update/index/2026.html` | Dated list of all releases (not used; per-product page is the keyed source) |
| Product index | `/en/1/product_data.xml` | name → product page href (could replace the hardcoded ids; not used yet) |

Product-page firmware row (inside `div#firmware`):

```html
<strong class="col">Z 9 Firmware</strong>
<span class="col title version"><span class="inner">C:Ver.5.33</span></span>
<span class="col title date">2026/10/20</span>
<span class="col title link"><a href="/en/download/fw/607.html">View download page</a></span>
```

Trap: the same page has a `#software` section whose rows use the identical `col title version`
class with values like `Full<br>Ver. 2.00` (NX Studio etc.). A class-only selector would
read software versions as firmware; the parser is scoped to `#firmware` and requires the `C:Ver.` prefix.

Version format: `C:Ver.5.33` — `C` is the camera ("C") firmware stream; Z 9 also has a GNSS "G"
stream that is updated through the same bundle and does not get its own row.
Minor is always two digits, matching `bcd_decode_version` (`0x0533` → `5.33`).

Field inventory (data-capture rule), product page firmware row + firmware page:

| Field | Disposition |
|---|---|
| Row title ("Z 9 Firmware") | extracted (used to reject a row for the wrong product) |
| Version `C:Ver.5.33` | extracted → `FwVersion` |
| Released `2026/10/20` | extracted (stored as `2026-10-20`) |
| Firmware-page href | extracted → `page_url` (absolute) |
| "Supplementary Firmware Update Manual" rows, software rows, manuals | dropped-with-reason: not firmware bundles |
| Firmware-page sentence "update … firmware to version 5.33" | extracted — cross-check against the product-page version; mismatch ⇒ *could not determine* |
| Firmware-page release notes / cautions / per-OS instructions | raw-only (full page saved as `source-page.html` next to the archived bundle when one is archived) |
| Licence agreement text (`/en/fw/ex_agreement.html`) | raw-only, not fetched by the tool; user must read it |
| File size, checksum, file name | **not published anywhere in the static HTML** — see §3 |

## 3. The gate (stop condition for automatic download)

The firmware page says: *"This software update program … is provided only on acceptance of the
agreement listed below. By selecting 'Accept' and clicking 'Download', you are held to have
accepted the terms and conditions of the agreement."* The agreement
(`/en/fw/ex_agreement.html`, loaded into the page by script) is a Nikon Corporation licence:
user must tick "I agree" and press "Begin download"; the OS (Windows / Mac OS) is also chosen on the page.
No download link, size, or checksum is present in the static HTML — the link is produced client-side
(`/common2/js/agreement-2.0.0.js`, `download-2.0.0.js`) after acceptance.

**Decision taken: the tool does not accept the licence and does not reverse-engineer the script to
reach the file without it.** `fleet firmware fetch` therefore stops at "manual download required",
prints the page URL, and offers `--from-file` to ingest the bundle the user downloaded themselves
after accepting (with provenance, hashing and the never-overwrite rule). Whether to automate
acceptance on the user's behalf is the user's call; nothing here forecloses it
(`DownloadAccess::Direct` is the slot a future source/decision would fill).

Also noted: the licence permits one working copy plus one backup copy on the user's own storage and
forbids redistribution — the archive must stay a private store, not be published or synced to shared locations.

## 4. Findings for the fleet today (2026-10-27)

| Model | Fleet reports | Latest published | Released |
|---|---|---|---|
| Z 9 | 5.31 (×2) | **5.33** (page 607) | 2026-10-20 |
| Z6_3 | 2.00 | **2.01** (page 574) | 2026-10-14 |
| Z 5 | 1.43 | 1.50 (page 562) | 2025-06-10 |

(5.32 for the Z 9 was already sitting loose in `firmware/Z_9_0532.bin`, unarchived.)

## 5. External-system assumptions (for the fresh-session reviewer)

| Assumption | Status |
|---|---|
| Product-page ids (589, 629, 552) are stable | Observed only today; a wrong id yields a title mismatch ⇒ *could not determine*, not silence |
| Firmware row title is `"<product> Firmware"` and `#firmware` holds exactly one such row per product | True for all three fixtures; >1 or 0 ⇒ *could not determine* |
| Versions are `C:Ver.N.NN` | Observed; anything else ⇒ *could not determine* |
| The site's "latest" = the single listed row | The site shows only the newest bundle; older ones are in `/en/archive.html` (unexplored) |
| Release notes "from X to Y" and the page sentence agree with the row | Cross-checked at parse time |
| HTTP 200 + this HTML shape stays; no bot-blocking | `/en/products/` and `/en/download/fw/` (directory URLs) return 403; real pages return 200. No evidence of rate limiting; we send ≤1 request/model/run |
| Licence permits personal archiving | Per the licence text read above (one copy + one backup); not legal advice |
