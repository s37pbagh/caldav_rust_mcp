//! MCP tool definitions and their implementations on top of the CalDAV client.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};
use url::Url;

use crate::caldav::{CalDavClient, CalendarObject, Precondition};
use crate::expand;
use crate::ical::{self, Component, Property, escape_text};

pub struct Tools {
    pub client: CalDavClient,
    pub default_calendar: Option<String>,
    pub default_timezone: Option<String>,
}

const CALENDAR_ARG: &str = "Calendar URL (from list_calendars) or display name. Defaults to CALDAV_DEFAULT_CALENDAR or the first event calendar.";
const TZ_ARG: &str = "IANA timezone (e.g. Europe/Berlin). Used for date-times given without an offset and for reporting results: returned times are converted to this zone with an explicit UTC offset. Defaults to CALDAV_TIMEZONE; without any timezone, times are reported in their stored zone.";

fn event_field_schema() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("summary".into(), json!({"type": "string", "description": "Event title."}));
    m.insert("start".into(), json!({"type": "string", "description": "Start: YYYY-MM-DD for all-day, YYYY-MM-DDTHH:MM[:SS] (uses timezone), or RFC 3339 with offset/Z."}));
    m.insert("end".into(), json!({"type": "string", "description": "End (exclusive), same formats as start. For all-day events the day after the last day. Defaults to 1 hour (or 1 day) after start; when only start is changed on update, the original duration is kept."}));
    m.insert("timezone".into(), json!({"type": "string", "description": TZ_ARG}));
    m.insert("description".into(), json!({"type": "string", "description": "Notes/body. Empty string removes it."}));
    m.insert("location".into(), json!({"type": "string", "description": "Location. Empty string removes it."}));
    m.insert("rrule".into(), json!({"type": "string", "description": "RFC 5545 recurrence rule, e.g. FREQ=WEEKLY;BYDAY=MO,WE;COUNT=10. Empty string removes it."}));
    m.insert("status".into(), json!({"type": "string", "enum": ["CONFIRMED", "TENTATIVE", "CANCELLED"]}));
    m.insert("attendees".into(), json!({"type": "array", "items": {"type": "string"}, "description": "Attendee emails, optionally as 'Name <email>'. Replaces existing attendees."}));
    m.insert("organizer".into(), json!({"type": "string", "description": "Organizer email (optionally 'Name <email>')."}));
    m.insert("categories".into(), json!({"type": "array", "items": {"type": "string"}, "description": "Categories/tags. Replaces existing ones."}));
    m.insert("reminder_minutes".into(), json!({"type": "array", "items": {"type": "integer", "minimum": 0}, "description": "Display reminders N minutes before start. Replaces existing reminders; [] removes all."}));
    m.insert("url".into(), json!({"type": "string", "description": "Related URL (e.g. meeting link)."}));
    m
}

fn locator_schema(m: &mut Map<String, Value>) {
    m.insert("event_url".into(), json!({"type": "string", "description": "Event URL as returned by list_events (preferred)."}));
    m.insert("uid".into(), json!({"type": "string", "description": "Event UID; used with calendar when event_url is not given."}));
    m.insert("calendar".into(), json!({"type": "string", "description": CALENDAR_ARG}));
}

pub fn definitions() -> Value {
    let mut create = event_field_schema();
    create.insert("calendar".into(), json!({"type": "string", "description": CALENDAR_ARG}));
    create.insert("uid".into(), json!({"type": "string", "description": "Optional UID; generated if omitted."}));

    let mut update = event_field_schema();
    locator_schema(&mut update);
    update.insert("etag".into(), json!({"type": "string", "description": "Optional etag for optimistic concurrency; defaults to the etag fetched just before updating."}));

    let mut get = Map::new();
    locator_schema(&mut get);
    get.insert("timezone".into(), json!({"type": "string", "description": TZ_ARG}));
    get.insert("include_raw".into(), json!({"type": "boolean", "description": "Include raw iCalendar data (default true)."}));

    let mut del = Map::new();
    locator_schema(&mut del);
    del.insert("etag".into(), json!({"type": "string", "description": "Only delete if the event still has this etag."}));

    json!([
        {
            "name": "list_calendars",
            "description": "List the user's CalDAV calendars with URL, name, color and supported component types.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "list_events",
            "description": "List events in a calendar overlapping a time range. Recurring events are expanded into individual occurrences by default. Returns event_url and etag for each event.",
            "inputSchema": {"type": "object", "properties": {
                "calendar": {"type": "string", "description": CALENDAR_ARG},
                "start": {"type": "string", "description": "Range start (date or date-time). Defaults to now."},
                "end": {"type": "string", "description": "Range end (date or date-time). Defaults to start + 7 days."},
                "timezone": {"type": "string", "description": TZ_ARG},
                "expand": {"type": "boolean", "description": "Expand recurring events into occurrences (default true)."},
                "query": {"type": "string", "description": "Optional case-insensitive text filter on summary, description and location."},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of events to return (default 200)."}
            }},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "get_event",
            "description": "Fetch a single event (all its components, e.g. a recurring master and overridden occurrences) by event_url or by uid + calendar.",
            "inputSchema": {"type": "object", "properties": get},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "create_event",
            "description": "Create a new event. Requires summary and start.",
            "inputSchema": {"type": "object", "properties": create, "required": ["summary", "start"]}
        },
        {
            "name": "update_event",
            "description": "Update fields of an existing event (identified by event_url or uid + calendar). Only the given fields change; all other data is preserved. For recurring events the whole series (master) is changed.",
            "inputSchema": {"type": "object", "properties": update}
        },
        {
            "name": "delete_event",
            "description": "Delete an event (the whole series for recurring events), identified by event_url or uid + calendar.",
            "inputSchema": {"type": "object", "properties": del},
            "annotations": {"destructiveHint": true}
        },
        {
            "name": "import_ics",
            "description": "Store raw iCalendar data (a VCALENDAR with one VEVENT/VTODO and its overrides) in a calendar. Fails if an object with the same UID exists unless overwrite is true.",
            "inputSchema": {"type": "object", "properties": {
                "calendar": {"type": "string", "description": CALENDAR_ARG},
                "ics": {"type": "string", "description": "Full iCalendar text starting with BEGIN:VCALENDAR."},
                "overwrite": {"type": "boolean", "description": "Replace an existing object with the same UID (default false)."}
            }, "required": ["ics"]}
        }
    ])
}

fn s<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str)
}

impl Tools {
    pub async fn call(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "list_calendars" => {
                let cals = self.client.list_calendars().await?;
                Ok(json!({ "calendars": cals }))
            }
            "list_events" => self.list_events(args).await,
            "get_event" => self.get_event(args).await,
            "create_event" => self.create_event(args).await,
            "update_event" => self.update_event(args).await,
            "delete_event" => self.delete_event(args).await,
            "import_ics" => self.import_ics(args).await,
            other => bail!("unknown tool '{other}'"),
        }
    }

    fn tz<'a>(&'a self, args: &'a Value) -> Option<&'a str> {
        s(args, "timezone").filter(|t| !t.is_empty()).or(self.default_timezone.as_deref())
    }

    /// Timezone used to report date-times.
    fn display_tz(&self, args: &Value) -> Result<Option<Tz>> {
        self.tz(args)
            .map(|t| ical::parse_tz(t).ok_or_else(|| anyhow!("unknown timezone '{t}'")))
            .transpose()
    }

    async fn calendar(&self, args: &Value) -> Result<Url> {
        self.client.resolve_calendar(s(args, "calendar"), self.default_calendar.as_deref()).await
    }

    async fn list_events(&self, args: &Value) -> Result<Value> {
        let cal = self.calendar(args).await?;
        let tz = self.tz(args);
        let display = self.display_tz(args)?;
        let start = match s(args, "start") {
            Some(v) => ical::to_utc_range_value(v, tz)?,
            None => ical::utc_stamp(Utc::now()),
        };
        let end = match s(args, "end") {
            Some(v) => ical::to_utc_range_value(v, tz)?,
            None => {
                let st = NaiveDateTime::parse_from_str(start.trim_end_matches('Z'), "%Y%m%dT%H%M%S")?;
                ical::utc_stamp((st + Duration::days(7)).and_utc())
            }
        };
        if end <= start {
            bail!("end must be after start");
        }
        let expand_recurring = args.get("expand").and_then(Value::as_bool).unwrap_or(true);
        let query = s(args, "query").map(str::to_lowercase).filter(|q| !q.is_empty());
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;

        let range_start = parse_utc_stamp(&start)?;
        let range_end = parse_utc_stamp(&end)?;
        // Servers can mis-evaluate time ranges for odd TZIDs (by up to ±14h), so query a
        // wider window and filter precisely after local expansion.
        let objects = self
            .client
            .events_in_range(
                cal.clone(),
                &ical::utc_stamp(range_start - Duration::days(1)),
                &ical::utc_stamp(range_end + Duration::days(1)),
            )
            .await?;
        let mut events: Vec<(i64, Value)> = Vec::new();
        let mut parse_errors = Vec::new();
        let mut warnings = Vec::new();
        for obj in &objects {
            let mut root = match ical::parse(&obj.data) {
                Ok(r) => r,
                Err(e) => {
                    parse_errors.push(format!("{}: {e}", obj.url));
                    continue;
                }
            };
            expand::normalize_fixed_offsets(&mut root, display);
            let items: Vec<Component> = if expand_recurring {
                let (items, w) = expand::expand(&root, range_start, range_end, display);
                warnings.extend(w);
                items
            } else {
                root.children.into_iter().filter(|c| c.name == "VEVENT").collect()
            };
            for ev in &items {
                if let Some(q) = &query {
                    let hay = ["SUMMARY", "DESCRIPTION", "LOCATION"]
                        .iter()
                        .filter_map(|p| ev.text(p))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .to_lowercase();
                    if !hay.contains(q.as_str()) {
                        continue;
                    }
                }
                let mut j = ical::event_to_json(ev, display);
                j["event_url"] = json!(obj.url.as_str());
                if let Some(e) = &obj.etag {
                    j["etag"] = json!(e);
                }
                events.push((ical::start_sort_key(ev, display), j));
            }
        }
        events.sort_by_key(|e| e.0);
        let total = events.len();
        let events: Vec<Value> = events.into_iter().take(limit).map(|(_, j)| j).collect();
        let mut out = json!({
            "calendar": cal.as_str(),
            "range": { "start": render_utc(&start, display), "end": render_utc(&end, display) },
            "count": events.len(),
            "events": events,
        });
        if total > limit {
            out["truncated"] = json!(format!("{total} events matched; only the first {limit} are shown"));
        }
        if !parse_errors.is_empty() {
            out["parse_errors"] = json!(parse_errors);
        }
        if !warnings.is_empty() {
            out["warnings"] = json!(warnings);
        }
        Ok(out)
    }

    /// Locate an event resource by event_url or uid (+ calendar).
    async fn locate(&self, args: &Value) -> Result<CalendarObject> {
        if let Some(u) = s(args, "event_url").filter(|u| !u.is_empty()) {
            return self.client.get(self.client.resolve(u)?).await;
        }
        let uid = s(args, "uid").filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("provide event_url or uid"))?;
        let cal = self.calendar(args).await?;
        let found = self
            .client
            .find_by_uid(cal.clone(), uid)
            .await?
            .ok_or_else(|| anyhow!("no event with UID '{uid}' in {cal}"))?;
        // Re-fetch to get the authoritative data and etag.
        match self.client.get(found.url.clone()).await {
            Ok(obj) => Ok(obj),
            Err(_) => Ok(found),
        }
    }

    async fn get_event(&self, args: &Value) -> Result<Value> {
        let obj = self.locate(args).await?;
        let mut root = ical::parse(&obj.data)?;
        let display = self.display_tz(args)?;
        expand::normalize_fixed_offsets(&mut root, display);
        let events: Vec<Value> = root.children.iter().filter(|c| c.name == "VEVENT").map(|e| ical::event_to_json(e, display)).collect();
        let mut out = json!({ "event_url": obj.url.as_str(), "etag": obj.etag, "events": events });
        if args.get("include_raw").and_then(Value::as_bool).unwrap_or(true) {
            out["ics"] = json!(obj.data);
        }
        Ok(out)
    }

    async fn create_event(&self, args: &Value) -> Result<Value> {
        let summary = s(args, "summary").filter(|v| !v.trim().is_empty()).ok_or_else(|| anyhow!("summary is required"))?;
        if s(args, "start").is_none() {
            bail!("start is required");
        }
        let cal = self.calendar(args).await?;
        let uid = s(args, "uid").filter(|u| !u.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{}@caldav-mcp", uuid::Uuid::new_v4()));
        let now = ical::utc_stamp(Utc::now());

        let mut ev = Component::new("VEVENT");
        ev.props.push(Property::new("UID", uid.clone()));
        ev.props.push(Property::new("DTSTAMP", now.clone()));
        ev.props.push(Property::new("CREATED", now.clone()));
        ev.props.push(Property::new("SUMMARY", escape_text(summary)));
        self.apply_fields(&mut ev, args, true)?;

        let mut root = Component::new("VCALENDAR");
        root.props.push(Property::new("VERSION", "2.0"));
        root.props.push(Property::new("PRODID", "-//caldav_mcp//EN"));
        root.props.push(Property::new("CALSCALE", "GREGORIAN"));
        root.children.push(ev);

        let url = cal.join(&format!("{}.ics", resource_name(&uid)))?;
        let etag = self.client.put(url.clone(), root.serialize(), Precondition::MustNotExist).await?;
        Ok(json!({
            "created": true,
            "event_url": url.as_str(),
            "etag": etag,
            "event": ical::event_to_json(&root.children[0], self.display_tz(args)?),
        }))
    }

    async fn update_event(&self, args: &Value) -> Result<Value> {
        let obj = self.locate(args).await?;
        let mut root = ical::parse(&obj.data)?;
        let ev = master_event(&mut root)?;
        if let Some(sum) = s(args, "summary") {
            ev.set(Property::new("SUMMARY", escape_text(sum)));
        }
        self.apply_fields(ev, args, false)?;
        let seq = ev.prop("SEQUENCE").and_then(|p| p.value.trim().parse::<i64>().ok()).unwrap_or(0);
        ev.set(Property::new("SEQUENCE", (seq + 1).to_string()));
        let now = ical::utc_stamp(Utc::now());
        ev.set(Property::new("DTSTAMP", now.clone()));
        ev.set(Property::new("LAST-MODIFIED", now));
        let event_json = ical::event_to_json(ev, self.display_tz(args)?);

        let etag = s(args, "etag").map(str::to_string).or(obj.etag.clone());
        let pre = match &etag {
            Some(e) => Precondition::Matches(e),
            None => Precondition::None,
        };
        let new_etag = self.client.put(obj.url.clone(), root.serialize(), pre).await?;
        Ok(json!({ "updated": true, "event_url": obj.url.as_str(), "etag": new_etag, "event": event_json }))
    }

    async fn delete_event(&self, args: &Value) -> Result<Value> {
        let (url, summary) = if let Some(u) = s(args, "event_url").filter(|u| !u.is_empty()) {
            (self.client.resolve(u)?, None)
        } else {
            let obj = self.locate(args).await?;
            let summary = ical::parse(&obj.data)
                .ok()
                .and_then(|r| r.children.iter().find(|c| c.name == "VEVENT").and_then(|e| e.text("SUMMARY")));
            (obj.url, summary)
        };
        self.client.delete(url.clone(), s(args, "etag")).await?;
        Ok(json!({ "deleted": true, "event_url": url.as_str(), "summary": summary }))
    }

    async fn import_ics(&self, args: &Value) -> Result<Value> {
        let ics = s(args, "ics").ok_or_else(|| anyhow!("ics is required"))?;
        let root = ical::parse(ics).context("ics is not valid iCalendar")?;
        if root.name != "VCALENDAR" {
            bail!("ics must be a VCALENDAR");
        }
        let item = root
            .children
            .iter()
            .find(|c| c.name != "VTIMEZONE")
            .ok_or_else(|| anyhow!("VCALENDAR contains no event or task"))?;
        let uid = item.prop("UID").map(|p| p.value.clone()).ok_or_else(|| anyhow!("component has no UID"))?;
        let cal = self.calendar(args).await?;
        let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
        // Reuse the existing resource URL if this UID already lives in the calendar.
        let existing = self.client.find_by_uid(cal.clone(), &uid).await.ok().flatten();
        let (url, pre) = match existing {
            Some(obj) if overwrite => (obj.url, Precondition::None),
            Some(obj) => bail!("an event with UID '{uid}' already exists at {} (set overwrite=true to replace it)", obj.url),
            None => (cal.join(&format!("{}.ics", resource_name(&uid)))?, if overwrite { Precondition::None } else { Precondition::MustNotExist }),
        };
        let etag = self.client.put(url.clone(), root.serialize(), pre).await?;
        Ok(json!({ "stored": true, "uid": uid, "event_url": url.as_str(), "etag": etag }))
    }

    /// Apply optional event fields from tool arguments to a VEVENT.
    fn apply_fields(&self, ev: &mut Component, args: &Value, creating: bool) -> Result<()> {
        let tz = self.tz(args);
        for (key, prop) in [("description", "DESCRIPTION"), ("location", "LOCATION")] {
            if let Some(v) = s(args, key) {
                if v.is_empty() { ev.remove(prop) } else { ev.set(Property::new(prop, escape_text(v))) }
            }
        }
        if let Some(v) = s(args, "url") {
            if v.is_empty() { ev.remove("URL") } else { ev.set(Property::new("URL", v)) }
        }
        if let Some(v) = s(args, "status") {
            let v = v.to_ascii_uppercase();
            if !["CONFIRMED", "TENTATIVE", "CANCELLED"].contains(&v.as_str()) {
                bail!("status must be CONFIRMED, TENTATIVE or CANCELLED");
            }
            ev.set(Property::new("STATUS", v));
        }
        if let Some(v) = s(args, "rrule") {
            let v = v.trim().trim_start_matches("RRULE:");
            if v.is_empty() { ev.remove("RRULE") } else { ev.set(Property::new("RRULE", v)) }
        }

        // Start / end.
        let old_start = ev.prop("DTSTART").cloned();
        let old_end = ev.prop("DTEND").cloned();
        let new_start = s(args, "start").map(|v| ical::dt_property("DTSTART", v, tz)).transpose()?;
        let new_end = s(args, "end").map(|v| ical::dt_property("DTEND", v, tz)).transpose()?;
        if let Some(st) = &new_start {
            ev.set(st.clone());
        }
        if let Some(en) = new_end {
            ev.remove("DURATION");
            ev.set(en);
        } else if creating {
            let all_day = new_start.as_ref().is_some_and(|p| p.param("VALUE") == Some("DATE"));
            ev.set(Property::new("DURATION", if all_day { "P1D" } else { "PT1H" }));
        } else if let (Some(ns), Some(os), Some(oe)) = (&new_start, &old_start, &old_end) {
            // Moving an event keeps its original length.
            if let (Some(os_v), Some(oe_v), Some(ns_v)) = (naive(&os.value), naive(&oe.value), naive(&ns.value)) {
                let shifted = ns_v.0 + (oe_v.0 - os_v.0);
                let mut end = ns.clone();
                end.name = "DTEND".into();
                end.value = if ns_v.1 {
                    shifted.date().format("%Y%m%d").to_string()
                } else {
                    format!("{}{}", shifted.format("%Y%m%dT%H%M%S"), if ns.value.ends_with('Z') { "Z" } else { "" })
                };
                ev.set(end);
            }
        }
        if let (Some(st), Some(en)) = (ev.prop("DTSTART"), ev.prop("DTEND"))
            && let (Some(a), Some(b)) = (naive(&st.value), naive(&en.value))
                && st.param("TZID") == en.param("TZID") && b.0 < a.0 {
                    bail!("end is before start");
                }

        if let Some(v) = s(args, "organizer") {
            if v.is_empty() { ev.remove("ORGANIZER") } else { ev.set(person_prop("ORGANIZER", v)) }
        }
        if let Some(list) = args.get("attendees").and_then(Value::as_array) {
            ev.remove("ATTENDEE");
            for a in list.iter().filter_map(Value::as_str) {
                ev.props.push(
                    person_prop("ATTENDEE", a)
                        .with_param("ROLE", "REQ-PARTICIPANT")
                        .with_param("PARTSTAT", "NEEDS-ACTION")
                        .with_param("RSVP", "TRUE"),
                );
            }
        }
        if let Some(list) = args.get("categories").and_then(Value::as_array) {
            ev.remove("CATEGORIES");
            let cats: Vec<String> = list.iter().filter_map(Value::as_str).map(escape_text).collect();
            if !cats.is_empty() {
                ev.props.push(Property::new("CATEGORIES", cats.join(",")));
            }
        }
        if let Some(list) = args.get("reminder_minutes").and_then(Value::as_array) {
            ev.children.retain(|c| c.name != "VALARM");
            let title = ev.text("SUMMARY").unwrap_or_else(|| "Reminder".into());
            for m in list.iter().filter_map(Value::as_u64) {
                let mut alarm = Component::new("VALARM");
                alarm.props.push(Property::new("ACTION", "DISPLAY"));
                alarm.props.push(Property::new("DESCRIPTION", escape_text(&title)));
                alarm.props.push(Property::new("TRIGGER", format!("-PT{m}M")));
                ev.children.push(alarm);
            }
        }
        Ok(())
    }
}

fn parse_utc_stamp(stamp: &str) -> Result<chrono::DateTime<Utc>> {
    Ok(NaiveDateTime::parse_from_str(stamp.trim_end_matches('Z'), "%Y%m%dT%H%M%S")?.and_utc())
}

/// Render a UTC stamp (YYYYMMDDTHHMMSSZ) in the display timezone.
fn render_utc(stamp: &str, display: Option<Tz>) -> String {
    ical::IcalTime::from_property(&Property::new("X", stamp))
        .map(|t| t.render(display).0)
        .unwrap_or_else(|| stamp.to_string())
}

/// Parse an iCalendar DATE or DATE-TIME value; returns (datetime, is_date).
fn naive(v: &str) -> Option<(NaiveDateTime, bool)> {
    let v = v.trim();
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y%m%d") {
        return Some((d.and_hms_opt(0, 0, 0)?, true));
    }
    NaiveDateTime::parse_from_str(v.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok().map(|d| (d, false))
}

/// The master VEVENT (no RECURRENCE-ID), or the first VEVENT.
fn master_event(root: &mut Component) -> Result<&mut Component> {
    let idx = root
        .children
        .iter()
        .position(|c| c.name == "VEVENT" && c.prop("RECURRENCE-ID").is_none())
        .or_else(|| root.children.iter().position(|c| c.name == "VEVENT"))
        .ok_or_else(|| anyhow!("resource contains no VEVENT"))?;
    Ok(&mut root.children[idx])
}

/// "Jane Doe <jane@example.com>" or "jane@example.com" -> CAL-ADDRESS property.
fn person_prop(name: &str, input: &str) -> Property {
    let input = input.trim();
    if let (Some(lt), Some(gt)) = (input.rfind('<'), input.rfind('>'))
        && lt < gt {
            let email = input[lt + 1..gt].trim();
            let cn = input[..lt].trim().trim_matches('"');
            let p = Property::new(name, format!("mailto:{email}"));
            return if cn.is_empty() { p } else { p.with_param("CN", &cn.replace('"', "")) };
        }
    Property::new(name, format!("mailto:{}", input.trim_start_matches("mailto:")))
}

/// A safe resource file name derived from a UID.
fn resource_name(uid: &str) -> String {
    if !uid.is_empty() && uid.len() <= 200 && uid.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c)) {
        uid.to_string()
    } else {
        uuid::Uuid::new_v4().to_string()
    }
}
