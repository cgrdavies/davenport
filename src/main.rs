//! MCP server exposing Apple iCloud Calendar (CalDAV) tools over Streamable HTTP.
//!
//! Transport: Streamable HTTP (rmcp) mounted at `/mcp`, guarded by a bearer token.
//! A plaintext `/health` endpoint is left unauthenticated for container health checks.

mod events;
mod icloud;
mod write;

use std::env;
use std::sync::Arc;

use chrono::{Duration, Utc};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use icloud::{ICLOUD_CALDAV_URL, Icloud, IcloudConfig};

// ---------------------------------------------------------------------------
// Tool argument schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ListEventsArgs {
    /// The calendar's CalDAV href (from `list_calendars`).
    calendar_href: String,
    /// Window start, RFC3339 (e.g. `2026-05-29T00:00:00Z`). Defaults to now.
    #[serde(default)]
    start: Option<String>,
    /// Window end, RFC3339. Defaults to 30 days after `start`.
    #[serde(default)]
    end: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetEventArgs {
    /// The event resource's CalDAV href (the `master_href` from `list_events`).
    master_href: String,
    /// When true, also return the original raw ICS for byte-exact fidelity.
    #[serde(default)]
    include_raw: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateInstanceArgs {
    /// The series resource's href (`master_href` from `list_events`).
    master_href: String,
    /// The resource's current ETag (`master_etag`) for a safe If-Match write.
    master_etag: String,
    /// The occurrence's original slot: pass its `recurrence_id` if set, otherwise
    /// its `start`. This identifies which occurrence to modify.
    recurrence_id: String,
    #[serde(default)]
    summary: Option<String>,
    /// New start, RFC3339 (or `YYYY-MM-DD` for all-day). Omit to keep the slot time.
    #[serde(default)]
    start: Option<String>,
    /// New end, RFC3339 (or `YYYY-MM-DD`). Omit to derive from the master duration.
    #[serde(default)]
    end: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    location: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteInstanceArgs {
    /// The series resource's href (`master_href` from `list_events`).
    master_href: String,
    /// The resource's current ETag (`master_etag`) for a safe If-Match write.
    master_etag: String,
    /// The occurrence's original slot: its `recurrence_id` if set, otherwise `start`.
    recurrence_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateEventArgs {
    /// The calendar's CalDAV href (from `list_calendars`).
    calendar_href: String,
    /// Event title.
    summary: String,
    /// Start time, RFC3339.
    start: String,
    /// End time, RFC3339.
    end: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    location: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateEventArgs {
    /// The event's CalDAV href (from `list_events`).
    event_href: String,
    /// The event UID (from `list_events`).
    uid: String,
    /// The event's current ETag, for a safe conditional write.
    etag: String,
    summary: String,
    /// Start time, RFC3339.
    start: String,
    /// End time, RFC3339.
    end: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    location: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteEventArgs {
    /// The event's CalDAV href (from `list_events`).
    event_href: String,
    /// The event's current ETag, for a safe conditional delete.
    etag: String,
}

// ---------------------------------------------------------------------------
// Tool output shapes (the upstream CalDAV types aren't Serialize)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CalendarOut {
    href: String,
    display_name: Option<String>,
    etag: Option<String>,
    sync_token: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CalendarServer {
    icloud: Icloud,
}

fn mcp_err(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value).map_err(mcp_err)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

#[tool_router]
impl CalendarServer {
    fn new(icloud: Icloud) -> Self {
        Self { icloud }
    }

    #[tool(description = "List the user's iCloud calendars (name + CalDAV href). \
        Use the returned href with the other tools.")]
    async fn list_calendars(&self) -> Result<CallToolResult, McpError> {
        let calendars = self.icloud.list_calendars().await.map_err(mcp_err)?;
        let out: Vec<CalendarOut> = calendars
            .into_iter()
            .map(|c| CalendarOut {
                href: c.href,
                display_name: c.displayname,
                etag: c.etag,
                sync_token: c.sync_token,
            })
            .collect();
        json_result(&out)
    }

    #[tool(description = "List calendar events as expanded, lightweight \
        occurrences within a [start, end] window (RFC3339; defaults to the next \
        30 days). Recurring series are expanded server-side (one entry per \
        occurrence, EXDATEs dropped, RECURRENCE-ID overrides applied). Each entry \
        is thin: uid, recurrence_id, start, end, all_day, summary, location, and \
        master_href/master_etag for addressing. Use get_event for full detail.")]
    async fn list_events(
        &self,
        Parameters(args): Parameters<ListEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve the window. Default to [now, now + 30d]; expansion needs a
        // bounded window or recurring series would be unbounded.
        let window_start = match args.start.as_deref() {
            Some(s) => chrono::DateTime::parse_from_rfc3339(s)
                .map_err(mcp_err)?
                .with_timezone(&Utc),
            None => Utc::now(),
        };
        let window_end = match args.end.as_deref() {
            Some(e) => chrono::DateTime::parse_from_rfc3339(e)
                .map_err(mcp_err)?
                .with_timezone(&Utc),
            None => window_start + Duration::days(30),
        };

        let objects = self
            .icloud
            .list_events(
                &args.calendar_href,
                args.start.as_deref(),
                args.end.as_deref(),
            )
            .await
            .map_err(mcp_err)?;
        // (master_href, master_etag, calendar_data) for resources that returned data.
        let resources: Vec<(String, Option<String>, String)> = objects
            .into_iter()
            .filter_map(|o| o.calendar_data.map(|data| (o.href, o.etag, data)))
            .collect();

        let occurrences = events::expand_occurrences(&resources, window_start, window_end);
        json_result(&occurrences)
    }

    #[tool(description = "Get full detail for a single event by its master_href \
        (from list_events): summary, description, location, status, start/end, \
        recurrence (rrule), organizer, attendees, and any RECURRENCE-ID overrides. \
        Set include_raw=true to also return the original ICS. This is the opt-in \
        depth path — use it before editing or to inspect, not for listing.")]
    async fn get_event(
        &self,
        Parameters(args): Parameters<GetEventArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (ics, _etag) = self
            .icloud
            .get_resource(&args.master_href)
            .await
            .map_err(mcp_err)?;
        let detail = events::project_detail(&ics, args.include_raw).map_err(mcp_err)?;
        json_result(&detail)
    }

    #[tool(description = "Edit a SINGLE occurrence of a recurring series without \
        touching the rest. Identify the occurrence by recurrence_id (its \
        recurrence_id from list_events if set, else its start). Creates or updates \
        a detached RECURRENCE-ID override; the master series is unchanged. Omitted \
        fields are left as-is; omit start/end to keep the slot's time. Uses \
        If-Match on master_etag. For non-recurring events use update_event.")]
    async fn update_event_instance(
        &self,
        Parameters(args): Parameters<UpdateInstanceArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Fetch current resource to mutate; If-Match uses the caller's etag so a
        // concurrent change is rejected (412) rather than silently overwritten.
        let (ics, _etag) = self
            .icloud
            .get_resource(&args.master_href)
            .await
            .map_err(mcp_err)?;
        let fields = write::InstanceFields {
            summary: args.summary,
            start: args.start,
            end: args.end,
            description: args.description,
            location: args.location,
        };
        let new_ics =
            write::apply_instance_update(&ics, &args.recurrence_id, &fields).map_err(mcp_err)?;
        let etag = self
            .icloud
            .put_ics_if_match(&args.master_href, new_ics, &args.master_etag)
            .await
            .map_err(mcp_err)?;
        json_result(&serde_json::json!({
            "master_href": args.master_href,
            "etag": etag,
            "updated_instance": args.recurrence_id,
        }))
    }

    #[tool(description = "Delete a SINGLE occurrence of a recurring series \
        (EXDATE) without touching the rest. Identify it by recurrence_id (its \
        recurrence_id from list_events if set, else its start). Also removes any \
        existing override for that slot. Uses If-Match on master_etag. For \
        non-recurring events use delete_event.")]
    async fn delete_event_instance(
        &self,
        Parameters(args): Parameters<DeleteInstanceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (ics, _etag) = self
            .icloud
            .get_resource(&args.master_href)
            .await
            .map_err(mcp_err)?;
        let new_ics = write::apply_instance_delete(&ics, &args.recurrence_id).map_err(mcp_err)?;
        let etag = self
            .icloud
            .put_ics_if_match(&args.master_href, new_ics, &args.master_etag)
            .await
            .map_err(mcp_err)?;
        json_result(&serde_json::json!({
            "master_href": args.master_href,
            "etag": etag,
            "deleted_instance": args.recurrence_id,
        }))
    }

    #[tool(description = "Create an event in a calendar. Times are RFC3339. \
        Returns the new event's href and etag.")]
    async fn create_event(
        &self,
        Parameters(args): Parameters<CreateEventArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (href, etag) = self
            .icloud
            .create_event(
                &args.calendar_href,
                &args.summary,
                &args.start,
                &args.end,
                args.description.as_deref(),
                args.location.as_deref(),
            )
            .await
            .map_err(mcp_err)?;
        json_result(&serde_json::json!({ "href": href, "etag": etag }))
    }

    #[tool(description = "Update an existing event (replace its contents) using \
        If-Match. Requires the event's uid and current etag from list_events.")]
    async fn update_event(
        &self,
        Parameters(args): Parameters<UpdateEventArgs>,
    ) -> Result<CallToolResult, McpError> {
        let etag = self
            .icloud
            .update_event(
                &args.event_href,
                &args.uid,
                &args.etag,
                &args.summary,
                &args.start,
                &args.end,
                args.description.as_deref(),
                args.location.as_deref(),
            )
            .await
            .map_err(mcp_err)?;
        json_result(&serde_json::json!({ "href": args.event_href, "etag": etag }))
    }

    #[tool(description = "Delete an event using If-Match. Requires the event's \
        current etag from list_events.")]
    async fn delete_event(
        &self,
        Parameters(args): Parameters<DeleteEventArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.icloud
            .delete_event(&args.event_href, &args.etag)
            .await
            .map_err(mcp_err)?;
        json_result(&serde_json::json!({ "deleted": args.event_href }))
    }
}

#[tool_handler]
impl ServerHandler for CalendarServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive]; build from Default and set fields.
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_03_26;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info =
            Implementation::new("davenport", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Apple iCloud Calendar over CalDAV. Call list_calendars first to \
             get a calendar href, then list/create/update/delete events. All \
             timestamps are RFC3339."
                .to_string(),
        );
        info
    }
}

// ---------------------------------------------------------------------------
// Bearer-token auth middleware (protects /mcp; /health stays open)
// ---------------------------------------------------------------------------

/// Extract a token from a `?token=` / `?access_token=` query string.
///
/// This is a fallback for clients that can't set request headers (e.g. adding
/// the server through a UI that only takes a URL, like Claude Desktop's custom
/// connector). Prefer the `Authorization: Bearer` header — query-string tokens
/// can end up in proxy/access logs. The token is a hex string so no
/// percent-decoding is needed.
fn token_from_query(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "token" || k == "access_token").then_some(v)
    })
}

async fn require_bearer(
    State(expected): State<Arc<String>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // Confine all borrows of `req` to this block so we can move it into
    // `next.run` afterwards.
    let authorized = {
        let header_token = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let provided = header_token.or_else(|| token_from_query(req.uri().query()));
        matches!(provided, Some(token) if token == expected.as_str())
    };
    if authorized {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let email = env::var("ICLOUD_EMAIL")
        .map_err(|_| anyhow::anyhow!("ICLOUD_EMAIL is required"))?;
    let password = env::var("ICLOUD_APP_SPECIFIC_PASSWORD")
        .map_err(|_| anyhow::anyhow!("ICLOUD_APP_SPECIFIC_PASSWORD is required"))?;
    let auth_token = env::var("MCP_AUTH_TOKEN")
        .map_err(|_| anyhow::anyhow!("MCP_AUTH_TOKEN is required (server is internet-facing)"))?;
    let base_url =
        env::var("ICLOUD_CALDAV_URL").unwrap_or_else(|_| ICLOUD_CALDAV_URL.to_string());
    let bind = env::var("MCP_BIND").unwrap_or_else(|_| "0.0.0.0:8000".to_string());

    let icloud = Icloud::new(IcloudConfig {
        base_url,
        email,
        password,
    });

    // DNS-rebinding protection: rmcp only accepts loopback Host headers by
    // default. Behind a reverse proxy on a public domain we must allow the
    // external hostname(s) via MCP_ALLOWED_HOSTS (comma-separated).
    let mut allowed_hosts: Vec<String> =
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    allowed_hosts.extend(
        env::var("MCP_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty()),
    );
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);

    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(CalendarServer::new(icloud.clone())),
        LocalSessionManager::default().into(),
        config,
    );

    let auth_token = Arc::new(auth_token);
    let mcp = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(auth_token, require_bearer));

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(mcp);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("davenport listening on http://{bind}/mcp");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
