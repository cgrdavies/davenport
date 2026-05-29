//! Thin wrapper around `fast-dav-rs` for talking to Apple iCloud CalDAV.
//!
//! iCloud is a standard CalDAV server: bootstrap at `https://caldav.icloud.com`,
//! authenticate with the Apple ID email + an app-specific password (basic auth),
//! then discover the principal and calendar-home-set.

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use fast_dav_rs::{CalDavClient, CalendarInfo, CalendarObject};
use std::sync::Arc;
use tokio::sync::OnceCell;

/// Default iCloud CalDAV bootstrap endpoint.
pub const ICLOUD_CALDAV_URL: &str = "https://caldav.icloud.com";

#[derive(Clone)]
pub struct IcloudConfig {
    pub base_url: String,
    pub email: String,
    pub password: String,
}

impl IcloudConfig {
    /// Build a fresh CalDAV client. Construction is cheap (just an HTTP client);
    /// the expensive part is discovery, which we cache on [`Icloud`].
    fn client(&self) -> Result<CalDavClient> {
        CalDavClient::new(&self.base_url, Some(&self.email), Some(&self.password))
            .context("failed to construct CalDAV client")
    }
}

/// Stateful iCloud handle that caches the discovered calendar-home path so we
/// don't re-run principal discovery on every tool call.
#[derive(Clone)]
pub struct Icloud {
    cfg: IcloudConfig,
    home: Arc<OnceCell<String>>,
}

impl Icloud {
    pub fn new(cfg: IcloudConfig) -> Self {
        Self {
            cfg,
            home: Arc::new(OnceCell::new()),
        }
    }

    /// Discover (once) and return the calendar-home-set path.
    async fn home(&self) -> Result<&str> {
        let home = self
            .home
            .get_or_try_init(|| async {
                let client = self.cfg.client()?;
                let principal = client
                    .discover_current_user_principal()
                    .await
                    .context("principal discovery failed")?
                    .ok_or_else(|| anyhow!("iCloud returned no current-user-principal"))?;
                let homes = client
                    .discover_calendar_home_set(&principal)
                    .await
                    .context("calendar-home-set discovery failed")?;
                homes
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("iCloud returned no calendar-home-set"))
            })
            .await?;
        Ok(home.as_str())
    }

    /// List the user's calendars.
    pub async fn list_calendars(&self) -> Result<Vec<CalendarInfo>> {
        let home = self.home().await?.to_string();
        let client = self.cfg.client()?;
        client
            .list_calendars(&home)
            .await
            .context("list_calendars failed")
    }

    /// List events in a calendar within an optional time window.
    /// `start`/`end` accept RFC3339 / ISO-8601 timestamps and are converted to
    /// the CalDAV basic-UTC form (`YYYYMMDDTHHMMSSZ`).
    pub async fn list_events(
        &self,
        calendar_href: &str,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Vec<CalendarObject>> {
        let client = self.cfg.client()?;
        let start = start.map(to_caldav_utc).transpose()?;
        let end = end.map(to_caldav_utc).transpose()?;
        client
            .calendar_query_timerange(
                calendar_href,
                "VEVENT",
                start.as_deref(),
                end.as_deref(),
                true,
            )
            .await
            .context("calendar_query_timerange failed")
    }

    /// Create a VEVENT in the given calendar. Returns the new event's href.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_event(
        &self,
        calendar_href: &str,
        summary: &str,
        start: &str,
        end: &str,
        description: Option<&str>,
        location: Option<&str>,
    ) -> Result<(String, Option<String>)> {
        let uid = uuid::Uuid::new_v4().to_string();
        let ics = build_vevent(&uid, summary, start, end, description, location)?;
        let href = join_href(calendar_href, &format!("{uid}.ics"));
        let client = self.cfg.client()?;
        let resp = client
            .put(&href, Bytes::from(ics))
            .await
            .context("PUT (create_event) failed")?;
        let etag = etag_of(&resp);
        Ok((href, etag))
    }

    /// Replace an existing VEVENT (identified by its href + UID) using
    /// If-Match for a safe conditional write.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_event(
        &self,
        event_href: &str,
        uid: &str,
        etag: &str,
        summary: &str,
        start: &str,
        end: &str,
        description: Option<&str>,
        location: Option<&str>,
    ) -> Result<Option<String>> {
        let ics = build_vevent(uid, summary, start, end, description, location)?;
        let client = self.cfg.client()?;
        let resp = client
            .put_if_match(event_href, Bytes::from(ics), etag)
            .await
            .context("conditional PUT (update_event) failed")?;
        Ok(etag_of(&resp))
    }

    /// Delete an event with an If-Match guard.
    pub async fn delete_event(&self, event_href: &str, etag: &str) -> Result<()> {
        let client = self.cfg.client()?;
        client
            .delete_if_match(event_href, etag)
            .await
            .context("conditional DELETE failed")?;
        Ok(())
    }
}

/// Convert an RFC3339/ISO-8601 timestamp to CalDAV basic-UTC (`YYYYMMDDTHHMMSSZ`).
fn to_caldav_utc(s: &str) -> Result<String> {
    let dt = DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("could not parse timestamp `{s}` as RFC3339"))?
        .with_timezone(&Utc);
    Ok(dt.format("%Y%m%dT%H%M%SZ").to_string())
}

/// Build a minimal but valid VCALENDAR/VEVENT document.
fn build_vevent(
    uid: &str,
    summary: &str,
    start: &str,
    end: &str,
    description: Option<&str>,
    location: Option<&str>,
) -> Result<String> {
    let dtstart = to_caldav_utc(start)?;
    let dtend = to_caldav_utc(end)?;
    let dtstamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let dtstamp = to_caldav_utc(&dtstamp)?;

    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//mcp-icloud-calendar-rs//EN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{dtstamp}"),
        format!("DTSTART:{dtstart}"),
        format!("DTEND:{dtend}"),
        format!("SUMMARY:{}", escape_ical(summary)),
    ];
    if let Some(d) = description {
        lines.push(format!("DESCRIPTION:{}", escape_ical(d)));
    }
    if let Some(l) = location {
        lines.push(format!("LOCATION:{}", escape_ical(l)));
    }
    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());
    // iCalendar lines are CRLF-delimited.
    Ok(lines.join("\r\n") + "\r\n")
}

/// Escape characters that are special in iCalendar text values (RFC 5545 §3.3.11).
fn escape_ical(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

/// Join a calendar collection href with a resource name, tolerating a missing
/// trailing slash.
fn join_href(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Pull the ETag header from a CalDAV response, if present.
fn etag_of<B>(resp: &http::Response<B>) -> Option<String> {
    resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}
