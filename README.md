# davenport

A small, self-contained **MCP server** that gives AI agents access to an **Apple iCloud Calendar** over CalDAV — exposed via **Streamable HTTP** so remote agents (claude.ai connectors, IDEs, etc.) can connect.

> Named after the writing desk — it's where your agents sit down to manage your calendar. (Also a nod to Cal**DAV**.)

Written in Rust: [`rmcp`](https://crates.io/crates/rmcp) (official MCP SDK) + [`fast-dav-rs`](https://crates.io/crates/fast-dav-rs) (async CalDAV client) + [`calcard`](https://crates.io/crates/calcard) (iCalendar parsing + RFC 5545 recurrence expansion + timezone resolution) + `axum`.

## Tools

A **two-tier read surface**: list thin/expanded by default, fetch full detail on demand.

| Tool | Description |
|------|-------------|
| `list_calendars` | List the user's calendars (name + CalDAV href). |
| `list_events` | **Tier 1.** Expanded, lightweight occurrences in a `[start, end]` window. |
| `get_event` | **Tier 2.** Full detail for one event by `master_href`; `include_raw` opt-in. |
| `create_event` | Create a VEVENT. Returns its href + etag. |
| `update_event` | Replace a whole event/series (If-Match; needs uid + etag). |
| `delete_event` | Delete a whole event/series (If-Match; needs etag). |
| `update_event_instance` | Edit **one occurrence** of a series via a detached RECURRENCE-ID override. |
| `delete_event_instance` | Delete **one occurrence** of a series (EXDATE). |

### Single-instance edits

`update_event_instance` / `delete_event_instance` change *one* occurrence
without mutating the master series. Identify the occurrence by `recurrence_id`
(its `recurrence_id` from `list_events` if set, else its `start`) and pass the
`master_href` + `master_etag`. We GET the resource, mutate it, and PUT it back
with `If-Match` on `master_etag` (a concurrent change → `412`, not a silent
overwrite):

- **Edit** adds (or updates) a detached VEVENT with the same UID, a
  `RECURRENCE-ID` = the occurrence's slot, and the new fields. Omitted fields are
  kept; omit `start`/`end` to keep the slot's time.
- **Delete** adds an `EXDATE` to the master and drops any override for that slot.

`RECURRENCE-ID`/`EXDATE` are written in the master's DTSTART representation
(same `TZID`, UTC, or `VALUE=DATE`) so the server matches the slot. The master's
`SEQUENCE` is mirrored onto new overrides (and never bumped on the master) so the
expander keeps pairing them. The whole-series `update_event`/`delete_event`
behaviour is unchanged.

### `list_events` (Tier 1)

Recurrences are expanded **server-side** for the requested window — one entry per
*occurrence*, not per recurring master. EXDATE'd dates are dropped and
`RECURRENCE-ID` overrides replace their generated slot. Window defaults to the
next 30 days. Each occurrence is intentionally thin:

```jsonc
{
  "uid": "…",                 // master event UID
  "recurring": true,          // part of a series (from a master RRULE, or a detached override)
  "recurrence_id": null,      // RFC 5545: present ONLY on a detached override (a VEVENT with a
                              // real RECURRENCE-ID); null for generated instances and one-offs
  "start": "2026-06-10T15:00:00-04:00",  // RFC3339+offset, or "YYYY-MM-DD" when all_day
  "end":   "2026-06-10T15:30:00-04:00",
  "all_day": false,
  "summary": "Standup",
  "location": "…",            // omitted when empty
  "master_href": "…",         // address the resource for get_event / writes
  "master_etag": "…"
}
```

`recurring` vs `recurrence_id` is the signal for single-instance editing:

| | `recurring` | `recurrence_id` | To edit just this occurrence |
|---|---|---|---|
| One-off | `false` | `null` | edit the master directly |
| Generated series instance | `true` | `null` | EXDATE the master at `start` + add a detached VEVENT |
| Detached override | `true` | the original slot | it's already standalone — edit it in place |

Expansion and timezone resolution (including Apple's custom `VTIMEZONE` blocks)
are handled by `calcard`; we don't depend on CalDAV server-side `<C:expand>`
(iCloud's support is flaky) — correctness comes from our own expansion.

### `get_event` (Tier 2)

Full fidelity for a single event addressed by `master_href`: summary,
description, location, status, start/end, `rrule`, organizer, attendees, and any
`RECURRENCE-ID` overrides. `include_raw` defaults to **false**; set it true to
also get the original ICS string. This is the opt-in depth path — pulled before
an edit or for inspection, never part of the default list flow.

All timestamps are RFC3339 (e.g. `2026-05-29T14:00:00Z`).

## Transport & auth

- MCP endpoint: `POST /mcp` (Streamable HTTP).
- Health check: `GET /health` → `ok` (unauthenticated, for container probes).
- **Every `/mcp` request must send `Authorization: Bearer <MCP_AUTH_TOKEN>`.** The
  server refuses to start without `MCP_AUTH_TOKEN` set, because it holds live
  read/write access to your calendar and is intended to be internet-facing.
- **Query-string fallback:** clients that can't set headers (e.g. a "custom
  connector" UI that only takes a URL) may pass the token as
  `…/mcp?token=<MCP_AUTH_TOKEN>` (`access_token` also accepted). Prefer the
  header — query strings can land in proxy/access logs. The header takes
  precedence when both are present.

## Configuration

See [`.env.example`](.env.example). Required:

- `ICLOUD_EMAIL` — your Apple ID email.
- `ICLOUD_APP_SPECIFIC_PASSWORD` — an app-specific password from
  [appleid.apple.com](https://appleid.apple.com) → *Sign-In and Security >
  App-Specific Passwords* (requires 2FA). Not your main password.
- `MCP_AUTH_TOKEN` — shared bearer secret clients must present.

Optional: `ICLOUD_CALDAV_URL` (default `https://caldav.icloud.com`),
`MCP_BIND` (default `0.0.0.0:8000`), `RUST_LOG` (default `info`).

## Run locally

```bash
cp .env.example .env   # fill in values
set -a; source .env; set +a
cargo run --release
```

## Docker / Dokploy

The image builds the release binary and runs it on port 8000. `docker-compose.yml`
is set up for Dokploy: it attaches to the external `dokploy-network` and uses
`expose` (Traefik handles routing — no host port binding).

```bash
docker compose up --build
```

## Connecting an agent

Point an MCP client at `https://<your-domain>/mcp` with header
`Authorization: Bearer <MCP_AUTH_TOKEN>`.
