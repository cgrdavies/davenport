//! Parse iCloud CalDAV calendar data and project it into a thin, **expanded**
//! occurrence list (Tier 1) and a full-fidelity single-event view (Tier 2).
//!
//! Recurrence expansion (RRULE/EXDATE/RECURRENCE-ID) and timezone resolution —
//! including Apple's custom `VTIMEZONE` blocks — are handled by the `calcard`
//! crate's [`ICalendar::expand_dates`]. We then filter to the requested window
//! and project each occurrence into the minimal shape the client needs.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use calcard::{
    Entry, Parser,
    common::timezone::Tz,
    icalendar::{ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarProperty},
};

/// Upper bound on occurrences generated per resource before window filtering.
/// `expand_dates` iterates from DTSTART (it has no built-in until-date), so this
/// caps pathological infinite/ancient series. A daily series stays in range for
/// ~50 years; weekly for centuries — comfortably more than any real window.
const EXPAND_LIMIT: usize = 20_000;

/// One expanded calendar occurrence — the Tier-1 default shape. Intentionally
/// thin: no raw ICS, no RRULE, no attendees. Use `get_event` for depth.
#[derive(Debug, Serialize, PartialEq)]
pub struct Occurrence {
    /// The master event's UID.
    pub uid: String,
    /// True when this occurrence belongs to a recurring series — i.e. it was
    /// generated from a master's RRULE, or is a detached override of one. False
    /// for plain one-off events. (Seriesness, for display/branching.)
    pub recurring: bool,
    /// The event's **actual** RECURRENCE-ID, present *only* on a detached
    /// override (a VEVENT individually modified out of the series). RFC 5545:
    /// generated instances have no RECURRENCE-ID, so this is `null` for them and
    /// for one-offs. Write-path signal: non-null ⇒ already a standalone override
    /// (edit it in place); null + `recurring` ⇒ synthesized from the master
    /// (edit just this one ⇒ EXDATE the master at `start` + add a detached
    /// VEVENT). The override's slot value is this field; a generated instance's
    /// slot is its own `start`.
    pub recurrence_id: Option<String>,
    /// RFC3339 with offset for timed events; a calendar date (`YYYY-MM-DD`) when
    /// `all_day` is true.
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Address the underlying resource for a later write.
    pub master_href: String,
    pub master_etag: Option<String>,
}

/// Tier-2 full-fidelity detail for a single event. `raw` is only populated when
/// the caller opts in (`include_raw = true`).
#[derive(Debug, Serialize, PartialEq)]
pub struct EventDetail {
    pub uid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    pub all_day: bool,
    pub recurring: bool,
    /// Raw RRULE line(s) for the master, e.g. `FREQ=WEEKLY;BYDAY=MO`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rrule: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizer: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
    /// Detached RECURRENCE-ID overrides present in the resource.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<OverrideInfo>,
    /// Original ICS text — only when `include_raw = true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct OverrideInfo {
    pub recurrence_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Parse a calendar-data blob into its VCALENDAR component(s).
fn parse_calendars(ics: &str) -> Vec<ICalendar> {
    let mut parser = Parser::new(ics);
    let mut out = Vec::new();
    loop {
        match parser.entry() {
            Entry::ICalendar(cal) => out.push(cal),
            Entry::Eof => break,
            // Skip vCards / malformed lines; keep scanning to EOF.
            _ => continue,
        }
    }
    out
}

/// Read a single text-valued property (SUMMARY, LOCATION, …).
fn text_prop(comp: &ICalendarComponent, prop: ICalendarProperty) -> Option<String> {
    comp.property(&prop)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_text())
        .map(str::to_string)
}

/// An event is all-day when its DTSTART is a DATE (no time component).
fn comp_all_day(comp: &ICalendarComponent) -> bool {
    comp.property(&ICalendarProperty::Dtstart)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_partial_date_time())
        .map(|pdt| !pdt.has_time())
        .unwrap_or(false)
}

/// Format a resolved datetime: a bare date for all-day, else RFC3339 + offset.
fn fmt_dt(dt: &DateTime<Tz>, all_day: bool) -> String {
    if all_day {
        dt.naive_local().date().format("%Y-%m-%d").to_string()
    } else {
        dt.fixed_offset().to_rfc3339_opts(SecondsFormat::Secs, false)
    }
}

/// The RECURRENCE-ID slot of an override component, resolved in `tz`.
fn override_recurrence_id(comp: &ICalendarComponent, tz: Tz, all_day: bool) -> Option<String> {
    comp.property(&ICalendarProperty::RecurrenceId)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_partial_date_time())
        .and_then(|pdt| pdt.to_date_time())
        .and_then(|dtr| dtr.to_date_time_with_tz(tz))
        .map(|dt| fmt_dt(&dt, all_day))
}

/// Expand all resources into a window-filtered, start-sorted occurrence list.
///
/// `objects` is `(master_href, master_etag, calendar_data)` per CalDAV resource.
pub fn expand_occurrences(
    objects: &[(String, Option<String>, String)],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<Occurrence> {
    let (ws, we) = (window_start.timestamp(), window_end.timestamp());
    let mut rows: Vec<(i64, Occurrence)> = Vec::new();

    for (href, etag, ics) in objects {
        for cal in parse_calendars(ics) {
            let expanded = cal.expand_dates(Tz::Tz(chrono_tz::UTC), EXPAND_LIMIT);
            for ev in expanded.events {
                // Resolve the (start, end) into concrete datetimes.
                let Some(ev) = ev.try_into_date_time() else {
                    continue;
                };
                let start_ts = ev.start.timestamp();
                let end_ts = ev.end.timestamp();
                // Keep occurrences that overlap [window_start, window_end).
                if start_ts >= we || end_ts <= ws {
                    continue;
                }
                let Some(comp) = cal.component_by_id(ev.comp_id) else {
                    continue;
                };

                let all_day = comp_all_day(comp);
                // RECURRENCE-ID is emitted ONLY for genuine detached overrides
                // (a VEVENT carrying a real RECURRENCE-ID). Generated instances
                // of a series have none — that distinction is the write-path
                // signal. Seriesness is reported separately via `recurring`.
                let recurring = comp.is_recurrent() || comp.is_recurrence_override();
                let recurrence_id = if comp.is_recurrence_override() {
                    override_recurrence_id(comp, ev.start.timezone(), all_day)
                } else {
                    None
                };

                rows.push((
                    start_ts,
                    Occurrence {
                        uid: comp.uid().unwrap_or_default().to_string(),
                        recurring,
                        recurrence_id,
                        start: fmt_dt(&ev.start, all_day),
                        end: fmt_dt(&ev.end, all_day),
                        all_day,
                        summary: text_prop(comp, ICalendarProperty::Summary).unwrap_or_default(),
                        location: text_prop(comp, ICalendarProperty::Location)
                            .filter(|s| !s.is_empty()),
                        master_href: href.clone(),
                        master_etag: etag.clone(),
                    },
                ));
            }
        }
    }

    rows.sort_by_key(|(ts, _)| *ts);
    rows.into_iter().map(|(_, occ)| occ).collect()
}

/// Is this a VEVENT (vs VTODO/VTIMEZONE/VALARM/…)?
fn is_vevent(comp: &ICalendarComponent) -> bool {
    comp.component_type == ICalendarComponentType::VEvent
}

/// Project a single resource's ICS into full-fidelity detail.
pub fn project_detail(ics: &str, include_raw: bool) -> Result<EventDetail> {
    let cal = parse_calendars(ics)
        .into_iter()
        .next()
        .context("resource contained no VCALENDAR")?;

    // The master is the VEVENT without a RECURRENCE-ID; the rest are overrides.
    let mut master: Option<&ICalendarComponent> = None;
    let mut overrides: Vec<OverrideInfo> = Vec::new();
    for comp in cal.components.iter().filter(|c| is_vevent(c)) {
        if comp.is_recurrence_override() {
            let all_day = comp_all_day(comp);
            overrides.push(OverrideInfo {
                recurrence_id: override_recurrence_id(comp, Tz::Tz(chrono_tz::UTC), all_day),
                summary: text_prop(comp, ICalendarProperty::Summary),
            });
        } else if master.is_none() {
            master = Some(comp);
        }
    }
    let master = master
        .or_else(|| cal.components.iter().find(|c| is_vevent(c)))
        .context("resource contained no VEVENT")?;

    let all_day = comp_all_day(master);

    // Resolve master start/end from the earliest expanded occurrence (this
    // gets timezone handling right, including custom VTIMEZONE).
    let (start, end) = cal
        .expand_dates(Tz::Tz(chrono_tz::UTC), EXPAND_LIMIT)
        .events
        .into_iter()
        .filter_map(|ev| ev.try_into_date_time())
        .filter(|ev| cal.component_by_id(ev.comp_id).is_some_and(|c| !c.is_recurrence_override()))
        .min_by_key(|ev| ev.start.timestamp())
        .map(|ev| (Some(fmt_dt(&ev.start, all_day)), Some(fmt_dt(&ev.end, all_day))))
        .unwrap_or((None, None));

    let rrule: Vec<String> = master
        .properties(&ICalendarProperty::Rrule)
        .filter_map(render_rrule)
        .collect();
    let attendees: Vec<String> = master
        .properties(&ICalendarProperty::Attendee)
        .filter_map(|e| e.values.first().and_then(|v| v.as_text()).map(str::to_string))
        .collect();

    Ok(EventDetail {
        uid: master.uid().unwrap_or_default().to_string(),
        summary: text_prop(master, ICalendarProperty::Summary),
        description: text_prop(master, ICalendarProperty::Description),
        location: text_prop(master, ICalendarProperty::Location).filter(|s| !s.is_empty()),
        status: master.status().map(|s| format!("{s:?}")),
        start,
        end,
        all_day,
        recurring: master.is_recurrent(),
        rrule,
        organizer: text_prop(master, ICalendarProperty::Organizer),
        attendees,
        overrides,
        raw: include_raw.then(|| ics.to_string()),
    })
}

/// Render an RRULE entry back to its `FREQ=…;…` form. Falls back to `None` if
/// the value isn't a recurrence rule (caller can still use `include_raw`).
fn render_rrule(entry: &calcard::icalendar::ICalendarEntry) -> Option<String> {
    use calcard::icalendar::ICalendarValue;
    match entry.values.first()? {
        ICalendarValue::RecurrenceRule(rule) => Some(format!("{rule:?}")),
        other => other.as_text().map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn window(start: &str, end: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        (
            DateTime::parse_from_rfc3339(start).unwrap().with_timezone(&Utc),
            DateTime::parse_from_rfc3339(end).unwrap().with_timezone(&Utc),
        )
    }

    fn obj(ics: &str) -> Vec<(String, Option<String>, String)> {
        vec![(
            "/cal/abc.ics".to_string(),
            Some("\"etag-1\"".to_string()),
            ics.to_string(),
        )]
    }

    #[test]
    fn one_off_timed_event() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:one-off\r\n\
            DTSTART:20260610T150000Z\r\nDTEND:20260610T160000Z\r\nSUMMARY:Solo\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let (s, e) = window("2026-06-01T00:00:00Z", "2026-06-30T00:00:00Z");
        let occ = expand_occurrences(&obj(ics), s, e);
        assert_eq!(occ.len(), 1);
        assert_eq!(occ[0].uid, "one-off");
        assert_eq!(occ[0].recurrence_id, None);
        assert!(!occ[0].recurring, "a one-off is not part of a series");
        assert!(!occ[0].all_day);
        assert_eq!(occ[0].summary, "Solo");
        assert_eq!(occ[0].master_href, "/cal/abc.ics");
        // 15:00Z must round-trip as an RFC3339 timestamp at that instant.
        let parsed = DateTime::parse_from_rfc3339(&occ[0].start).unwrap();
        assert_eq!(parsed.with_timezone(&Utc), Utc.with_ymd_and_hms(2026, 6, 10, 15, 0, 0).unwrap());
    }

    #[test]
    fn all_day_event() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:allday\r\n\
            DTSTART;VALUE=DATE:20260612\r\nDTEND;VALUE=DATE:20260613\r\nSUMMARY:Holiday\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let (s, e) = window("2026-06-01T00:00:00Z", "2026-06-30T00:00:00Z");
        let occ = expand_occurrences(&obj(ics), s, e);
        assert_eq!(occ.len(), 1);
        assert!(occ[0].all_day, "DATE-valued DTSTART should be all_day");
        assert_eq!(occ[0].start, "2026-06-12");
        assert_eq!(occ[0].recurrence_id, None);
        assert!(!occ[0].recurring);
    }

    #[test]
    fn weekly_recurrence_with_exdates() {
        // Weekly on Wednesdays from Jun 3; exclude Jun 10 and Jun 24.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:weekly\r\n\
            DTSTART:20260603T150000Z\r\nDTEND:20260603T153000Z\r\nSUMMARY:Standup\r\n\
            RRULE:FREQ=WEEKLY;COUNT=6\r\n\
            EXDATE:20260610T150000Z\r\nEXDATE:20260624T150000Z\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let (s, e) = window("2026-06-01T00:00:00Z", "2026-07-31T00:00:00Z");
        let occ = expand_occurrences(&obj(ics), s, e);
        // COUNT=6 minus 2 EXDATEs = 4 occurrences.
        assert_eq!(occ.len(), 4, "got: {occ:#?}");
        let starts: Vec<&str> = occ.iter().map(|o| o.start.as_str()).collect();
        assert!(starts.iter().all(|s| !s.starts_with("2026-06-10")), "Jun 10 excluded");
        assert!(starts.iter().all(|s| !s.starts_with("2026-06-24")), "Jun 24 excluded");
        // Generated instances are flagged `recurring` but carry NO recurrence_id
        // (they have no RECURRENCE-ID property — only real overrides do).
        assert!(occ.iter().all(|o| o.recurring), "all are series instances");
        assert!(
            occ.iter().all(|o| o.recurrence_id.is_none()),
            "generated instances must not have a RECURRENCE-ID"
        );
        assert!(occ.iter().all(|o| o.uid == "weekly"));
    }

    #[test]
    fn detached_recurrence_id_override() {
        // Weekly series; the Jun 10 instance is detached and retitled/moved.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
            BEGIN:VEVENT\r\nUID:series\r\nDTSTART:20260603T150000Z\r\nDTEND:20260603T153000Z\r\n\
            SUMMARY:Standup\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:series\r\nRECURRENCE-ID:20260610T150000Z\r\n\
            DTSTART:20260610T173000Z\r\nDTEND:20260610T180000Z\r\nSUMMARY:Standup (moved)\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let (s, e) = window("2026-06-01T00:00:00Z", "2026-07-31T00:00:00Z");
        let occ = expand_occurrences(&obj(ics), s, e);
        assert_eq!(occ.len(), 4, "4 weekly slots, one replaced by the override: {occ:#?}");
        // The override replaces the generated Jun 10 slot.
        let moved = occ.iter().find(|o| o.summary == "Standup (moved)").expect("override present");
        assert!(moved.start.starts_with("2026-06-10T17:30"), "moved to 17:30: {}", moved.start);
        // The override carries a REAL recurrence_id = the ORIGINAL slot (15:00),
        // not its new start. This is the one occurrence that should have it.
        assert!(moved.recurring);
        assert_eq!(moved.recurrence_id.as_deref(), Some("2026-06-10T15:00:00+00:00"));
        // The generated (non-override) siblings are recurring but have NO recurrence_id.
        let generated: Vec<_> = occ.iter().filter(|o| o.summary == "Standup").collect();
        assert_eq!(generated.len(), 3, "3 generated weekly slots remain");
        assert!(
            generated.iter().all(|o| o.recurring && o.recurrence_id.is_none()),
            "generated siblings: recurring=true, recurrence_id=null"
        );
        // No generated 15:00 Jun 10 occurrence remains.
        assert!(
            !occ.iter().any(|o| o.start.starts_with("2026-06-10T15:00")),
            "original Jun 10 slot must be replaced, not duplicated"
        );
    }

    #[test]
    fn get_event_detail_default_no_raw() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:weekly\r\n\
            DTSTART:20260603T150000Z\r\nDTEND:20260603T153000Z\r\nSUMMARY:Standup\r\n\
            DESCRIPTION:Daily sync\r\nRRULE:FREQ=WEEKLY;COUNT=6\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let detail = project_detail(ics, false).unwrap();
        assert_eq!(detail.uid, "weekly");
        assert_eq!(detail.summary.as_deref(), Some("Standup"));
        assert!(detail.recurring);
        assert!(!detail.rrule.is_empty(), "rrule should be surfaced");
        assert!(detail.raw.is_none(), "raw omitted by default");
        assert!(detail.start.is_some());

        let with_raw = project_detail(ics, true).unwrap();
        assert!(with_raw.raw.is_some(), "raw included when requested");
    }
}
