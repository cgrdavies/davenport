//! MCP server exposing Apple iCloud Calendar (CalDAV) tools over Streamable HTTP.
//!
//! Transport: Streamable HTTP (rmcp) mounted at `/mcp`, guarded by a bearer token.
//! A plaintext `/health` endpoint is left unauthenticated for container health checks.

mod icloud;

use std::env;
use std::sync::Arc;

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
        StreamableHttpService, session::local::LocalSessionManager,
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
    /// Optional window start, RFC3339 (e.g. `2026-05-29T00:00:00Z`).
    #[serde(default)]
    start: Option<String>,
    /// Optional window end, RFC3339.
    #[serde(default)]
    end: Option<String>,
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

#[derive(Serialize)]
struct EventOut {
    href: String,
    etag: Option<String>,
    status: Option<String>,
    calendar_data: Option<String>,
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

    #[tool(description = "List events in a calendar, optionally within a time \
        window (RFC3339 start/end). Returns raw iCalendar VEVENT data plus href/etag.")]
    async fn list_events(
        &self,
        Parameters(args): Parameters<ListEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let events = self
            .icloud
            .list_events(&args.calendar_href, args.start.as_deref(), args.end.as_deref())
            .await
            .map_err(mcp_err)?;
        let out: Vec<EventOut> = events
            .into_iter()
            .map(|e| EventOut {
                href: e.href,
                etag: e.etag,
                status: e.status,
                calendar_data: e.calendar_data,
            })
            .collect();
        json_result(&out)
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

async fn require_bearer(
    State(expected): State<Arc<String>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match provided {
        Some(token) if token == expected.as_str() => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
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

    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(CalendarServer::new(icloud.clone())),
        LocalSessionManager::default().into(),
        Default::default(),
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
