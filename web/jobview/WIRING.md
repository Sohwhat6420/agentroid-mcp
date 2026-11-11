# Job view wiring (2.11b → bridged)

This is what bridged has to serve for the job view to work live. Today bridged has no HTTP listener: it speaks only on its named pipe, and 2.11a is not merged. So everything here is the **contract the page is built and tested against**, not a description of running code.

`scripts/mock-bridged.mjs` implements this file exactly. `scripts/interact.mjs` drives the built page against it, so the mock is the reference test for 2.11a's HTTP half.

Nothing in `crates/bridged` was changed.

## 1. Bind rules

| Mode | Listens on | Everything else |
|---|---|---|
| Default | `127.0.0.1` only (one port, chosen at start and written next to the pipe name) | — |
| Hub mode | `127.0.0.1` **and** the tailnet interface address (100.64.0.0/10, or the `fd7a:115c:a1e0::/48` IPv6 range) | — |
| LAN | Never | A request from any other source address gets a 403, or the socket is not bound at all |

In hub mode, bind the tailnet interface address, never `0.0.0.0`.

The project's measured check for the LAN rule is: "A LAN probe to the view is refused". The mock refuses every non-loopback client.

**One origin.** The page and every endpoint below come from the same origin, so the page makes no cross-origin request and needs no CORS. The checks confirm 0 requests leave the origin: the shots, a11y, axe and interact scripts route and log every request.

## 2. Static files

`npm run build` writes `web/jobview/dist/`. bridged serves that folder at `/`, with relative asset URLs (`base: './'`).

| Path | Type | Notes |
|---|---|---|
| `/` → `index.html` | `text/html; charset=utf-8` | bridged inserts `<meta name="agentroid-token" content="…">` straight after `<head>` (section 5) |
| `/theme-boot.js` | `text/javascript` | sets the theme before first paint; a file rather than inline script, so CSP can stay strict |
| `/assets/index-*.js`, `/assets/index-*.css` | | hashed; may be cached for a year |
| `/assets/*.woff2` | `font/woff2` | Geist 400/600 and JetBrains Mono (SIL OFL; licences in `src/fonts/`). No third-party font host |
| `/fixtures/**` | json, ndjson, jpeg | recorded scenarios for `?fixture=` (466 KB). bridged **may omit** this folder in production; `?fixture=` then shows an honest "could not be read" |

**Headers on every response:**

```
Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self'; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'
X-Content-Type-Options: nosniff
Cache-Control: no-store            (everything except /assets/*)
```

The policy allows `style-src-attr 'unsafe-inline'` because the page writes per-row colours and positions as `style=""` attributes on markup it has already escaped.
- `<style>` elements and stylesheets stay `'self'` only, and scripts stay `'self'` only.
- This was measured: with plain `style-src 'self'`, Chromium blocked those attributes.

## 3. Endpoints

`:id` is the job id. All payloads are UTF-8 JSON unless stated otherwise.

| Method and path | Answer | Schema | How the page uses it |
|---|---|---|---|
| `GET /jobs` | the job list | `schemas/job-view.json` (array form) | With no `?job=`, the page opens the newest unfinished job, otherwise the newest job. Re-read every 30 s for "Other jobs on this laptop". |
| `GET /jobs/:id` | one job | `schemas/job-view.json` (object form), including `changes_in_force` from 2.13 | Polled every 5 s. **The HTTP `Date` header is required**: the page takes its clock skew from it, so every age it shows is measured on the laptop's clock. |
| `GET /jobs/:id/events?after=<seq>` with `Accept: application/x-ndjson` | the NDJSON backlog after `seq` (all of it without `after`) | `schemas/progress.json`, one object per line | Read once on load, then every 2 s only while the stream is down. |
| `GET /jobs/:id/events` with `Accept: text/event-stream` | SSE | each `data:` is exactly one NDJSON line | `id:` is the event's `seq`. On reconnect, honour `Last-Event-ID` and resend only later events. Optional `event: job` frames carry the job object, so a status change shows without waiting for the 5 s poll. |
| `GET /jobs/:id/frame.json` | latest frame metadata, or 404 before the first frame | `contract/frame-meta.schema.json` (**proposed**) | Read at most once per 30 s. A new `ts` means a new picture. |
| `GET /jobs/:id/frame.jpg?ts=<ts>` | the JPEG, ≤ 61,440 bytes | | Fetched only when `ts` changes, so it can be cached by URL. Never served for a secure window. |
| `GET /jobs/:id/recorder.json` | flight-recorder summary, or 404 if none | `contract/recorder.schema.json` (**proposed**) | Read when the job turns `stuck` or `error`. |
| `GET /jobs/:id/recorder/crop.jpg` | the one crop (also used in the push), ≤ 61,440 bytes | | Shown in the step detail. |
| `POST /jobs/:id/control` | `202` with `{"accepted":true}` | body `{"action": "pause" \| "resume" \| "take_over" \| "hand_back" \| "retry" \| "stop"}` | Section 5. |

### SSE versus polling

The page uses SSE when `EventSource` connects. If the stream errors, the link becomes `poll`: the page reads the backlog at once and every 2 s until the stream returns. Events are deduplicated by `seq`, so a replay is harmless.

The link counts as **down** after 15 s with no successful answer. The page then says "Stale: …" and how old the data is, and greys the frame. Every fetch times out after 8 s.

A hidden tab stops polling, and the page catches up when it is shown again.

## 4. Payloads

- **Job:** `schemas/job-view.json`, unchanged.
  - Status maps to what the page shows:

    | Job status | Page shows |
    |---|---|
    | `running` | running, or cooling while a heat pause is open |
    | `paused` | a word chosen by the pause message (below) |
    | `taken_over` | "You have the phone" |
    | `stuck` | stuck |
    | `stopped` | stopped |
    | `done` | done |
    | `error` | "Failed" |
    | `queued` | queued |

  - `paused` uses the open `pause` event's `msg` prefix:

    | `msg` prefix | Page shows |
    |---|---|
    | `heat:` | cooling |
    | `needs_attention:` or `awaiting_human:` | "Waiting for you" |
    | `paused:` with `paused_by_user` | "Paused by you" |
    | anything else | "Paused" |
- **Events:** `schemas/progress.json`, unchanged. The fields the page relies on:
  - `seq` (dedupe and order);
  - `ts` (all times);
  - `kind`;
  - `step_id` and `msg`. A step message ending in `: ok` is shown without that suffix; `attempt N of M failed: …` counts a retry;
  - `temp_c` and `batt` (readings; none are invented);
  - `eta_s` (used only until 5 steps have finished, after which the page measures its own).
- **Frame metadata (proposed):** `contract/frame-meta.schema.json`.
  - Required: `v` (1), `run_id`, `ts`, `bytes` (0..61,440), the JPEG's `w` and `h`, the real screen's `screen_w` and `screen_h`, and `secure`.
  - `secure: true` requires `bytes: 0`: no picture at all.
  - Optional: `display`, `step_id`;
    - `next_tap {step_id, x, y, selector, label?}` drives the ghost cursor;
    - `miss {step_id, x, y, selector, reason}` drives the crossed ring when stuck.
  - Coordinates are in screen pixels. `additionalProperties` is false.
- **Recorder (proposed):** `contract/recorder.schema.json`.
  - Fields: `v`, `run_id`, `ts`, `step_id`, `reason`, `trees` (0..10), `frames` (0..3), and `crop {url, x, y, w, h, bytes ≤ 61,440}`.
  - `crop.url` must match `^/jobs/[^/]+/recorder/crop\.jpg$`.

**Validation at the edge.** Every payload passes a hand-written JSON Schema checker (`src/contract/schema.ts`) built from these schema files before the page uses it; there is no second copy of the rules.
- A schema keyword the checker does not implement throws at load. It is never silently ignored.
- A payload that fails is counted and described in words, and never used:
  - a bad job keeps the last good one and says how old it is;
  - a bad event line is left out, listed under Raw events, and the band says so;
  - a bad frame shows "Frame unavailable: …".
- An event or frame from another `run_id` is dropped.

## 5. Controls (proposed; nothing in bridged implements them yet)

`POST /jobs/:id/control` with:
- header `X-Agentroid-Token: <token>`. The token is the per-start secret bridged injected into `index.html` (`<meta name="agentroid-token">`), compared in constant time.
- `Origin`, when present, matching `http://<Host>`. Otherwise answer 403. This stops another page in the same browser from driving the phone.

| Answer | Meaning | What the page shows |
|---|---|---|
| `202` | accepted; the page re-reads the job at once | the button shows "Pausing…" (dashed, busy) until the status changes; if nothing changes within 10 s, it says so |
| `404`, `405`, `501` | controls not wired | "Controls are not wired on this laptop yet. Use the phone tile, or ask Claude." |
| `403` | token or origin refused | "Refused: this page is not allowed to control the job (token or origin check)." |
| `409` | wrong state | "Refused: the job is no longer in a state that allows …" |

Stop always asks first. Focus lands on "Keep running", and Esc cancels.

## 6. Gaps in the schemas (proposed, not added to `schemas/`)

1. **`progress.json` has no pause reason field.** The page reads the reason from the `msg` prefix that `engine.rs` writes: `"{state}: {detail}{at}; lease kept"`, with the detail codes from `popups.rs` `PauseReason::detail`. A `reason` enum would be sturdier.
2. **No `powered` field.** The design says the battery floor uses `powered`, not "charging". The page states the floor and the 80% cap, but cannot say "on USB power".
3. **No frame history.** Only the latest frame exists, so scrubbing back through the flight recorder shows step data and the one recorder crop, not old pictures.
4. **`temp_c` and `batt` are `None` today** in `engine.rs` `event()`. Until 2.13 fills them, the page says "No reading". It never draws a made-up line.
5. **`frame.json` and `recorder.json`** need adding to `schemas/` with fixtures when 2.11a lands. The two files in `contract/` are ready to move.

## 7. Modes in the URL

| URL | Source |
|---|---|
| `/` | live: the newest unfinished job from `GET /jobs` |
| `/?job=<id>` | live: that job |
| `/?fixture=<name>[&freeze=1]` | recorded data from `fixtures/<name>/`, labelled **FIXTURE** in the header and footer. The clock runs from the fixture's recorded "now", or stays still with `freeze=1` |
| `&theme=light\|dark` | overrides the remembered or system theme |
| `&filter=problems\|pauses` | the step list's filter (shareable) |
| `#step-<n>` | opens that step |
