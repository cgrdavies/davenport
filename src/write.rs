//! Single-instance write path for recurring series.
//!
//! Editing or deleting *one* occurrence of a recurring series must never mutate
//! the master VEVENT (that would change every instance). Instead, per RFC 5545:
//!
//! - **Edit one occurrence** → add (or update) a detached override VEVENT: same
//!   UID, a `RECURRENCE-ID` equal to the occurrence's original slot, and the new
//!   DTSTART/DTEND/SUMMARY/… The override shadows the generated instance.
//! - **Delete one occurrence** → add an `EXDATE` for that slot to the master and
//!   drop any existing override for it.
//!
//! Critical correctness detail: `RECURRENCE-ID` and `EXDATE` are written in the
//! **same representation as the master's DTSTART** (same `TZID`, or UTC `Z`, or
//! `VALUE=DATE`). iCloud matches the slot by that representation; a UTC EXDATE
//! against a `TZID`-anchored series may silently fail to match.
//!
//! We parse → mutate the component tree → re-serialize with `calcard` (Stalwart's
//! production iCalendar writer), preserving VTIMEZONE and everything we don't
//! touch. The whole-series `update_event`/`delete_event` tools are unaffected.

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};

use calcard::{
    Entry, Parser,
    common::PartialDateTime,
    icalendar::{
        ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarParameter,
        ICalendarProperty, ICalendarValue,
    },
};

/// Fields an agent may change on a single occurrence. `None` means "leave as is".
#[derive(Debug, Default)]
pub struct InstanceFields {
    pub summary: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
}

/// How the master expresses its date-times — we mirror this for RECURRENCE-ID,
/// EXDATE, and the override's DTSTART/DTEND.
#[derive(Debug, Clone)]
enum DateStyle {
    AllDay,
    Utc,
    Tzid(String),
    Floating,
}

/// Parse the first VCALENDAR out of a resource.
fn parse_one(ics: &str) -> Result<ICalendar> {
    let mut parser = Parser::new(ics);
    loop {
        match parser.entry() {
            Entry::ICalendar(cal) => return Ok(cal),
            Entry::Eof => bail!("resource contained no VCALENDAR"),
            _ => continue,
        }
    }
}

fn is_vevent(c: &ICalendarComponent) -> bool {
    c.component_type == ICalendarComponentType::VEvent
}

/// Index of the VCALENDAR root (the writer treats component 0 as root).
fn root_index(cal: &ICalendar) -> usize {
    cal.components
        .iter()
        .position(|c| c.component_type == ICalendarComponentType::VCalendar)
        .unwrap_or(0)
}

/// Index of the series master: the VEVENT without a RECURRENCE-ID.
fn master_index(cal: &ICalendar) -> Result<usize> {
    cal.components
        .iter()
        .position(|c| is_vevent(c) && !c.is_recurrence_override())
        .or_else(|| cal.components.iter().position(is_vevent))
        .context("resource contained no VEVENT")
}

/// Derive the master's date representation from its DTSTART.
fn master_style(master: &ICalendarComponent) -> DateStyle {
    let entry = master.property(&ICalendarProperty::Dtstart);
    let pdt = entry
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_partial_date_time());
    match (entry, pdt) {
        (Some(e), Some(p)) if p.has_time() => match e.tz_id() {
            Some(tz) => DateStyle::Tzid(tz.to_string()),
            None if p.tz_hour.is_some() => DateStyle::Utc,
            None => DateStyle::Floating,
        },
        (_, Some(p)) if !p.has_time() => DateStyle::AllDay,
        _ => DateStyle::Utc,
    }
}

/// Comparable wall-clock key from a PartialDateTime (tz-agnostic — both the
/// generated slot and an override's RECURRENCE-ID live in the master's tz).
fn pdt_key(p: &PartialDateTime) -> (u16, u8, u8, u8, u8, u8) {
    (
        p.year.unwrap_or(0),
        p.month.unwrap_or(0),
        p.day.unwrap_or(0),
        p.hour.unwrap_or(0),
        p.minute.unwrap_or(0),
        p.second.unwrap_or(0),
    )
}

/// Parse an agent-supplied timed value (RFC3339, or naive `YYYY-MM-DDTHH:MM:SS`)
/// into the wall clock we should store for `style`.
fn parse_timed(input: &str, style: &DateStyle) -> Result<NaiveDateTime> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(input) {
        return Ok(match style {
            // UTC series: store the instant's UTC wall clock.
            DateStyle::Utc => dt.with_timezone(&Utc).naive_utc(),
            // TZID/floating: store the local wall clock as given.
            _ => dt.naive_local(),
        });
    }
    NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S")
        .with_context(|| format!("could not parse `{input}` as RFC3339 or naive date-time"))
}

fn naive_to_pdt(n: NaiveDateTime, utc: bool) -> PartialDateTime {
    PartialDateTime {
        year: Some(n.year() as u16),
        month: Some(n.month() as u8),
        day: Some(n.day() as u8),
        hour: Some(n.hour() as u8),
        minute: Some(n.minute() as u8),
        second: Some(n.second() as u8),
        tz_hour: utc.then_some(0),
        tz_minute: utc.then_some(0),
        tz_minus: false,
    }
}

fn date_to_pdt(d: NaiveDate) -> PartialDateTime {
    PartialDateTime {
        year: Some(d.year() as u16),
        month: Some(d.month() as u8),
        day: Some(d.day() as u8),
        ..Default::default()
    }
}

/// Build the `(value, params)` for a date-time in the master's representation.
fn build_value(input: &str, style: &DateStyle) -> Result<(PartialDateTime, Vec<ICalendarParameter>)> {
    match style {
        DateStyle::AllDay => {
            let d = NaiveDate::parse_from_str(input.get(..10).unwrap_or(input), "%Y-%m-%d")
                .with_context(|| format!("all-day series needs a `YYYY-MM-DD` value, got `{input}`"))?;
            Ok((date_to_pdt(d), vec![ICalendarParameter::value("DATE".to_string())]))
        }
        DateStyle::Utc => Ok((naive_to_pdt(parse_timed(input, style)?, true), vec![])),
        DateStyle::Tzid(tz) => Ok((
            naive_to_pdt(parse_timed(input, style)?, false),
            vec![ICalendarParameter::tzid(tz.clone())],
        )),
        DateStyle::Floating => Ok((naive_to_pdt(parse_timed(input, style)?, false), vec![])),
    }
}

/// Master event's duration (used to derive an occurrence's end when the caller
/// only moves the start). Falls back to 0 (timed) / 1 day (all-day).
fn master_duration(master: &ICalendarComponent, style: &DateStyle) -> Duration {
    let read = |prop| {
        master
            .property(prop)
            .and_then(|e| e.values.first())
            .and_then(|v| v.as_partial_date_time())
            .and_then(|p| p.to_date_time())
            .map(|d| d.date_time)
    };
    match (read(&ICalendarProperty::Dtstart), read(&ICalendarProperty::Dtend)) {
        (Some(s), Some(e)) => e - s,
        _ => match style {
            DateStyle::AllDay => Duration::days(1),
            _ => Duration::zero(),
        },
    }
}

/// Add a value+params for `prop`, replacing any existing entries of that name.
fn set_prop(
    comp: &mut ICalendarComponent,
    prop: ICalendarProperty,
    pdt: PartialDateTime,
    params: Vec<ICalendarParameter>,
) {
    comp.entries.retain(|e| e.name != prop);
    comp.entries.push(
        ICalendarEntry::new(prop)
            .with_value(ICalendarValue::PartialDateTime(Box::new(pdt)))
            .with_params(params),
    );
}

fn set_text(comp: &mut ICalendarComponent, prop: ICalendarProperty, text: &str) {
    comp.entries.retain(|e| e.name != prop);
    comp.entries
        .push(ICalendarEntry::new(prop).with_value(ICalendarValue::Text(text.to_string())));
}

fn text_of(comp: &ICalendarComponent, prop: ICalendarProperty) -> Option<String> {
    comp.property(&prop)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_text())
        .map(str::to_string)
}

/// Refresh DTSTAMP. We deliberately do NOT touch SEQUENCE here: an override is
/// linked to its master by `(SEQUENCE, RECURRENCE-ID)` (this is how our own
/// expander, calcard, pairs them), so a new override must copy the master's
/// SEQUENCE — see `set_sequence` — and the master's SEQUENCE must stay put or
/// existing overrides would unlink. CalDAV PUT acceptance is governed by
/// If-Match, not SEQUENCE.
fn set_dtstamp(comp: &mut ICalendarComponent) {
    comp.entries.retain(|e| e.name != ICalendarProperty::Dtstamp);
    comp.entries.push(
        ICalendarEntry::new(ICalendarProperty::Dtstamp).with_value(ICalendarValue::PartialDateTime(
            Box::new(PartialDateTime::from_utc_timestamp(Utc::now().timestamp())),
        )),
    );
}

fn sequence_of(comp: &ICalendarComponent) -> Option<i64> {
    comp.property(&ICalendarProperty::Sequence)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_integer())
}

/// Make an override's SEQUENCE match the master's so the expander pairs them.
fn set_sequence(comp: &mut ICalendarComponent, seq: Option<i64>) {
    comp.entries.retain(|e| e.name != ICalendarProperty::Sequence);
    if let Some(s) = seq {
        comp.entries
            .push(ICalendarEntry::new(ICalendarProperty::Sequence).with_value(ICalendarValue::Integer(s)));
    }
}

/// An override's RECURRENCE-ID as a comparable wall-clock key.
fn override_key(comp: &ICalendarComponent) -> Option<(u16, u8, u8, u8, u8, u8)> {
    comp.property(&ICalendarProperty::RecurrenceId)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_partial_date_time())
        .map(pdt_key)
}

/// Modify a single occurrence. `slot` is the occurrence's original RECURRENCE-ID
/// (for a generated instance pass its `start`; for an existing override pass its
/// `recurrence_id`). Returns the new ICS to PUT back with If-Match.
pub fn apply_instance_update(ics: &str, slot: &str, fields: &InstanceFields) -> Result<String> {
    let mut cal = parse_one(ics)?;
    let root = root_index(&cal);
    let m_idx = master_index(&cal)?;
    let style = master_style(&cal.components[m_idx]);
    let uid = cal.components[m_idx]
        .uid()
        .context("master VEVENT has no UID")?
        .to_string();
    let dur = master_duration(&cal.components[m_idx], &style);
    let master_seq = sequence_of(&cal.components[m_idx]);
    let master_summary = text_of(&cal.components[m_idx], ICalendarProperty::Summary);
    let master_desc = text_of(&cal.components[m_idx], ICalendarProperty::Description);
    let master_loc = text_of(&cal.components[m_idx], ICalendarProperty::Location);

    let (slot_pdt, slot_params) = build_value(slot, &style)?;
    let slot_key = pdt_key(&slot_pdt);

    // Resolve the occurrence's new start/end values.
    let start_input = fields.start.clone().unwrap_or_else(|| slot.to_string());
    let end_input = match fields.end.clone() {
        Some(e) => e,
        None => derive_end(&start_input, dur, &style)?,
    };
    let (start_pdt, start_params) = build_value(&start_input, &style)?;
    let (end_pdt, end_params) = build_value(&end_input, &style)?;

    // Is there already a detached override for this slot?
    let existing = cal
        .components
        .iter()
        .position(|c| is_vevent(c) && c.is_recurrence_override() && override_key(c) == Some(slot_key));

    if let Some(i) = existing {
        let comp = &mut cal.components[i];
        set_prop(comp, ICalendarProperty::Dtstart, start_pdt, start_params);
        set_prop(comp, ICalendarProperty::Dtend, end_pdt, end_params);
        if let Some(s) = &fields.summary {
            set_text(comp, ICalendarProperty::Summary, s);
        }
        if let Some(d) = &fields.description {
            set_text(comp, ICalendarProperty::Description, d);
        }
        if let Some(l) = &fields.location {
            set_text(comp, ICalendarProperty::Location, l);
        }
        set_dtstamp(comp);
    } else {
        let mut comp = ICalendarComponent::new(ICalendarComponentType::VEvent);
        comp.add_uid(&uid);
        set_prop(&mut comp, ICalendarProperty::RecurrenceId, slot_pdt, slot_params);
        set_prop(&mut comp, ICalendarProperty::Dtstart, start_pdt, start_params);
        set_prop(&mut comp, ICalendarProperty::Dtend, end_pdt, end_params);
        if let Some(s) = fields.summary.clone().or(master_summary) {
            set_text(&mut comp, ICalendarProperty::Summary, &s);
        }
        if let Some(d) = fields.description.clone().or(master_desc) {
            set_text(&mut comp, ICalendarProperty::Description, &d);
        }
        if let Some(l) = fields.location.clone().or(master_loc) {
            set_text(&mut comp, ICalendarProperty::Location, &l);
        }
        set_sequence(&mut comp, master_seq);
        set_dtstamp(&mut comp);
        let idx = cal.components.len() as u32;
        cal.components.push(comp);
        cal.components[root].component_ids.push(idx);
    }

    Ok(cal.to_string())
}

/// Delete a single occurrence: EXDATE the slot on the master and drop any
/// existing override for it. Returns the new ICS to PUT back with If-Match.
pub fn apply_instance_delete(ics: &str, slot: &str) -> Result<String> {
    let mut cal = parse_one(ics)?;
    let m_idx = master_index(&cal)?;
    let style = master_style(&cal.components[m_idx]);
    if !cal.components[m_idx].is_recurrent() {
        bail!("event is not a recurring series; use delete_event to remove it entirely");
    }
    let (slot_pdt, slot_params) = build_value(slot, &style)?;
    let slot_key = pdt_key(&slot_pdt);

    // EXDATE on the master.
    let master = &mut cal.components[m_idx];
    master.entries.push(
        ICalendarEntry::new(ICalendarProperty::Exdate)
            .with_value(ICalendarValue::PartialDateTime(Box::new(slot_pdt)))
            .with_params(slot_params),
    );
    set_dtstamp(master);

    // Drop any override for the same slot.
    let to_remove: Vec<u32> = cal
        .components
        .iter()
        .enumerate()
        .filter(|(_, c)| is_vevent(c) && c.is_recurrence_override() && override_key(c) == Some(slot_key))
        .map(|(i, _)| i as u32)
        .collect();
    if !to_remove.is_empty() {
        cal.remove_component_ids(&to_remove);
    }

    Ok(cal.to_string())
}

/// new_end = new_start + master duration, formatted for `style`.
fn derive_end(start_input: &str, dur: Duration, style: &DateStyle) -> Result<String> {
    match style {
        DateStyle::AllDay => {
            let d = NaiveDate::parse_from_str(start_input.get(..10).unwrap_or(start_input), "%Y-%m-%d")
                .with_context(|| format!("all-day value must be `YYYY-MM-DD`, got `{start_input}`"))?;
            let days = dur.num_days().max(1);
            Ok((d + Duration::days(days)).format("%Y-%m-%d").to_string())
        }
        _ => {
            let start = parse_timed(start_input, style)?;
            let end = start + dur;
            // Re-emit in a form parse_timed accepts. For UTC we tag the instant.
            match style {
                DateStyle::Utc => Ok(Utc.from_utc_datetime(&end).to_rfc3339()),
                _ => Ok(end.format("%Y-%m-%dT%H:%M:%S").to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::expand_occurrences;
    use chrono::DateTime;

    const WEEKLY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:series\r\n\
        DTSTART:20260603T150000Z\r\nDTEND:20260603T153000Z\r\nSUMMARY:Standup\r\n\
        RRULE:FREQ=WEEKLY;COUNT=4\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn occ(ics: &str) -> Vec<crate::events::Occurrence> {
        let objs = vec![("/c/x.ics".to_string(), Some("\"e\"".to_string()), ics.to_string())];
        let s = DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z").unwrap().with_timezone(&Utc);
        let e = DateTime::parse_from_rfc3339("2026-07-15T00:00:00Z").unwrap().with_timezone(&Utc);
        expand_occurrences(&objs, s, e)
    }

    #[test]
    fn update_generated_instance_creates_override() {
        // Move the Jun 10 15:00Z occurrence to 17:30Z and retitle it.
        let fields = InstanceFields {
            summary: Some("Standup (moved)".into()),
            start: Some("2026-06-10T17:30:00Z".into()),
            end: Some("2026-06-10T18:00:00Z".into()),
            ..Default::default()
        };
        let new_ics = apply_instance_update(WEEKLY, "2026-06-10T15:00:00Z", &fields).unwrap();
        let occ = occ(&new_ics);
        assert_eq!(occ.len(), 4, "still 4 slots, one overridden: {occ:#?}");
        let moved = occ.iter().find(|o| o.summary == "Standup (moved)").expect("override present");
        assert!(moved.start.starts_with("2026-06-10T17:30"), "moved: {}", moved.start);
        assert!(moved.recurrence_id.is_some(), "override carries a real RECURRENCE-ID");
        assert!(!occ.iter().any(|o| o.start.starts_with("2026-06-10T15:00")), "old slot replaced, not duplicated");
        // Other instances untouched.
        assert!(occ.iter().any(|o| o.start.starts_with("2026-06-03T15:00") && o.summary == "Standup"));
    }

    #[test]
    fn second_update_modifies_same_override_in_place() {
        let f1 = InstanceFields { summary: Some("v1".into()), start: Some("2026-06-10T17:30:00Z".into()), end: Some("2026-06-10T18:00:00Z".into()), ..Default::default() };
        let ics1 = apply_instance_update(WEEKLY, "2026-06-10T15:00:00Z", &f1).unwrap();
        let f2 = InstanceFields { summary: Some("v2".into()), ..Default::default() };
        let ics2 = apply_instance_update(&ics1, "2026-06-10T15:00:00Z", &f2).unwrap();
        // Exactly one VEVENT carries RECURRENCE-ID — we updated, didn't add a second.
        let n_overrides = ics2.matches("RECURRENCE-ID").count();
        assert_eq!(n_overrides, 1, "must reuse the existing override\n{ics2}");
        let occ = occ(&ics2);
        assert!(occ.iter().any(|o| o.summary == "v2"));
        assert!(!occ.iter().any(|o| o.summary == "v1"));
    }

    #[test]
    fn delete_instance_adds_exdate() {
        let new_ics = apply_instance_delete(WEEKLY, "2026-06-10T15:00:00Z").unwrap();
        assert!(new_ics.contains("EXDATE"), "EXDATE added:\n{new_ics}");
        let occ = occ(&new_ics);
        assert_eq!(occ.len(), 3, "one slot removed: {occ:#?}");
        assert!(!occ.iter().any(|o| o.start.starts_with("2026-06-10")), "Jun 10 gone");
        // The rest remain.
        assert!(occ.iter().any(|o| o.start.starts_with("2026-06-03")));
        assert!(occ.iter().any(|o| o.start.starts_with("2026-06-17")));
    }

    #[test]
    fn delete_then_override_gone_too() {
        // First create an override on Jun 10, then delete that occurrence.
        let f = InstanceFields { summary: Some("moved".into()), start: Some("2026-06-10T17:30:00Z".into()), end: Some("2026-06-10T18:00:00Z".into()), ..Default::default() };
        let ics1 = apply_instance_update(WEEKLY, "2026-06-10T15:00:00Z", &f).unwrap();
        let ics2 = apply_instance_delete(&ics1, "2026-06-10T15:00:00Z").unwrap();
        let occ = occ(&ics2);
        assert_eq!(occ.len(), 3, "deleted occurrence (incl. its override) gone");
        assert!(!occ.iter().any(|o| o.start.starts_with("2026-06-10")));
        assert!(!occ.iter().any(|o| o.summary == "moved"));
    }

    #[test]
    fn tzid_series_writes_exdate_with_tzid() {
        // A TZID-anchored weekly series: EXDATE must carry the same TZID.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:tz\r\n\
            DTSTART;TZID=America/New_York:20260603T090000\r\nDTEND;TZID=America/New_York:20260603T093000\r\n\
            SUMMARY:Sync\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        // Slot exposed to the agent would be 09:00-04:00 (EDT).
        let new_ics = apply_instance_delete(ics, "2026-06-10T09:00:00-04:00").unwrap();
        assert!(
            new_ics.contains("EXDATE;TZID=America/New_York:20260610T090000"),
            "EXDATE must mirror the master TZID/wall-clock:\n{new_ics}"
        );
    }
}
