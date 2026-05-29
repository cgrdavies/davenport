# mcp-icloud-calendar-rs

A small, self-contained **MCP server** that gives AI agents access to an **Apple iCloud Calendar** over CalDAV — exposed via **Streamable HTTP** so remote agents (claude.ai connectors, IDEs, etc.) can connect.

Written in Rust: [`rmcp`](https://crates.io/crates/rmcp) (official MCP SDK) + [`fast-dav-rs`](https://crates.io/crates/fast-dav-rs) (async CalDAV client) + `axum`.

## Tools

| Tool | Description |
|------|-------------|
| `list_calendars` | List the user's calendars (name + CalDAV href). |
| `list_events` | List events in a calendar, optional RFC3339 time window. |
| `create_event` | Create a VEVENT. Returns its href + etag. |
| `update_event` | Replace an event (If-Match safe write; needs uid + etag). |
| `delete_event` | Delete an event (If-Match; needs etag). |

All timestamps are RFC3339 (e.g. `2026-05-29T14:00:00Z`).

## Transport & auth

- MCP endpoint: `POST /mcp` (Streamable HTTP).
- Health check: `GET /health` → `ok` (unauthenticated, for container probes).
- **Every `/mcp` request must send `Authorization: Bearer <MCP_AUTH_TOKEN>`.** The
  server refuses to start without `MCP_AUTH_TOKEN` set, because it holds live
  read/write access to your calendar and is intended to be internet-facing.

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
