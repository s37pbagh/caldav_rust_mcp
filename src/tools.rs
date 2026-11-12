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
    pub default_task_list: Option<String>,
    pub default_timezone: Option<String>,
}

/// The kind of calendar object a tool works on.
#[derive(Clone, Copy)]
struct Kind {
    /// iCalendar component name.
    comp: &'static str,
    /// Tool argument / result key holding the resource URL.
    url_key: &'static str,
    noun: &'static str,
}

const EVENT: Kind = Kind { comp: "VEVENT", url_key: "event_url", noun: "event" };
const TASK: Kind = Kind { comp: "VTODO", url_key: "task_url", noun: "task" };

const CALENDAR_ARG: &str = "Calendar URL (from list_calendars) or display name. Defaults to CALDAV_DEFAULT_CALENDAR or the first event calendar.";
const TASK_LIST_ARG: &str = "Calendar / task list URL (from list_calendars) or display name. Defaults to CALDAV_DEFAULT_TASK_LIST, then CALDAV_DEFAULT_CALENDAR if it supports tasks, then the first calendar that supports tasks (VTODO).";
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

fn task_field_schema() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("summary".into(), json!({"type": "string", "description": "Task title."}));
    m.insert("due".into(), json!({"type": "string", "description": "Due: YYYY-MM-DD (date only), YYYY-MM-DDTHH:MM[:SS] (uses timezone), or RFC 3339 with offset/Z. Empty string removes it."}));
    m.insert("start".into(), json!({"type": "string", "description": "Start (when work on the task begins), same formats as due. Empty string removes it."}));
    m.insert("timezone".into(), json!({"type": "string", "description": TZ_ARG}));
    m.insert("description".into(), json!({"type": "string", "description": "Notes/body. Empty string removes it."}));
    m.insert("location".into(), json!({"type": "string", "description": "Location. Empty string removes it."}));
    m.insert("completed".into(), json!({"type": "boolean", "description": "true marks the task done (STATUS:COMPLETED, completion time, 100%); false reopens it."}));
    m.insert("status".into(), json!({"type": "string", "enum": ["NEEDS-ACTION", "IN-PROCESS", "COMPLETED", "CANCELLED"]}));
    m.insert("priority".into(), json!({"type": "integer", "minimum": 0, "maximum": 9, "description": "1 = highest, 5 = medium, 9 = lowest, 0 = none (removes it)."}));
    m.insert("percent_complete".into(), json!({"type": "integer", "minimum": 0, "maximum": 100}));
    m.insert("parent_uid".into(), json!({"type": "string", "description": "UID of the parent task, making this a subtask. Empty string removes the link."}));
    m.insert("rrule".into(), json!({"type": "string", "description": "RFC 5545 recurrence rule, e.g. FREQ=WEEKLY;BYDAY=MO. Empty string removes it."}));
    m.insert("categories".into(), json!({"type": "array", "items": {"type": "string"}, "description": "Categories/tags. Replaces existing ones."}));
    m.insert("reminder_minutes".into(), json!({"type": "array", "items": {"type": "integer", "minimum": 0}, "description": "Display reminders N minutes before due (or before start when there is no due). Replaces existing reminders; [] removes all."}));
    m.insert("url".into(), json!({"type": "string", "description": "Related URL."}));
    m
}

fn locator_schema(m: &mut Map<String, Value>, kind: Kind) {
    let (list_tool, cal_arg) = if kind.comp == "VTODO" { ("list_tasks", TASK_LIST_ARG) } else { ("list_events", CALENDAR_ARG) };
    m.insert(kind.url_key.into(), json!({"type": "string", "description": format!("URL of the {} as returned by {list_tool} (preferred).", kind.noun)}));
    m.insert("uid".into(), json!({"type": "string", "description": format!("UID of the {}; used with calendar when {} is not given.", kind.noun, kind.url_key)}));
    m.insert("calendar".into(), json!({"type": "string", "description": cal_arg}));
}

pub fn definitions() -> Value {
    let mut create = event_field_schema();
    create.insert("calendar".into(), json!({"type": "string", "description": CALENDAR_ARG}));
    create.insert("uid".into(), json!({"type": "string", "description": "Optional UID; generated if omitted."}));

    let mut update = event_field_schema();
    locator_schema(&mut update, EVENT);
    update.insert("etag".into(), json!({"type": "string", "description": "Optional etag for optimistic concurrency; defaults to the etag fetched just before updating."}));

    let mut get = Map::new();
    locator_schema(&mut get, EVENT);
    get.insert("timezone".into(), json!({"type": "string", "description": TZ_ARG}));
    get.insert("include_raw".into(), json!({"type": "boolean", "description": "Include raw iCalendar data (default true)."}));

    let mut del = Map::new();
    locator_schema(&mut del, EVENT);
    del.insert("etag".into(), json!({"type": "string", "description": "Only delete if the event still has this etag."}));

    let mut create_task = task_field_schema();
    create_task.insert("calendar".into(), json!({"type": "string", "description": TASK_LIST_ARG}));
    create_task.insert("uid".into(), json!({"type": "string", "description": "Optional UID; generated if omitted."}));

    let mut update_task = task_field_schema();
    locator_schema(&mut update_task, TASK);
    update_task.insert("etag".into(), json!({"type": "string", "description": "Optional etag for optimistic concurrency; defaults to the etag fetched just before updating."}));

    let mut get_task = Map::new();
    locator_schema(&mut get_task, TASK);
    get_task.insert("timezone".into(), json!({"type": "string", "description": TZ_ARG}));
    get_task.insert("include_raw".into(), json!({"type": "boolean", "description": "Include raw iCalendar data (default true)."}));

    let mut del_task = Map::new();
    locator_schema(&mut del_task, TASK);
    del_task.insert("etag".into(), json!({"type": "string", "description": "Only delete if the task still has this etag."}));

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
            "name": "list_tasks",
            "description": "List tasks (VTODO / to-dos / reminders) in a calendar or task list. By default only open tasks are returned, sorted by due date (tasks without a due date last), then priority. Returns task_url and etag for each task.",
            "inputSchema": {"type": "object", "properties": {
                "calendar": {"type": "string", "description": TASK_LIST_ARG},
                "include_completed": {"type": "boolean", "description": "Also return completed and cancelled tasks (default false)."},
                "due_after": {"type": "string", "description": "Only tasks due at or after this date/date-time. Tasks without a due date are excluded when due_after or due_before is given."},
                "due_before": {"type": "string", "description": "Only tasks due before this date/date-time (e.g. tomorrow's date for 'due today or overdue')."},
                "timezone": {"type": "string", "description": TZ_ARG},
                "query": {"type": "string", "description": "Optional case-insensitive text filter on summary, description, location and categories."},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of tasks to return (default 200)."}
            }},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "get_task",
            "description": "Fetch a single task by task_url or by uid + calendar.",
            "inputSchema": {"type": "object", "properties": get_task},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "create_task",
            "description": "Create a new task (VTODO). Requires summary.",
            "inputSchema": {"type": "object", "properties": create_task, "required": ["summary"]}
        },
        {
            "name": "update_task",
            "description": "Update fields of an existing task (identified by task_url or uid + calendar). Only the given fields change; all other data is preserved. Use completed=true to mark a task done. For recurring tasks the whole series is changed.",
            "inputSchema": {"type": "object", "properties": update_task}
        },
        {
            "name": "delete_task",
            "description": "Delete a task, identified by task_url or uid + calendar.",
            "inputSchema": {"type": "object", "properties": del_task},
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
            "delete_event" => self.delete(args, EVENT).await,
            "import_ics" => self.import_ics(args).await,
            "list_tasks" => self.list_tasks(args).await,
            "get_task" => self.get_task(args).await,
            "create_task" => self.create_task(args).await,
            "update_task" => self.update_task(args).await,
            "delete_task" => self.delete(args, TASK).await,
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

    async fn calendar(&self, args: &Value, kind: Kind) -> Result<Url> {
        let defaults: &[Option<&str>] = if kind.comp == "VTODO" {
            &[self.default_task_list.as_deref(), self.default_calendar.as_deref()]
        } else {
            &[self.default_calendar.as_deref()]
        };
        self.client.resolve_calendar(s(args, "calendar"), defaults, kind.comp).await
    }

    async fn list_events(&self, args: &Value) -> Result<Value> {
        let cal = self.calendar(args, EVENT).await?;
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

    /// Locate an event/task resource by its URL or uid (+ calendar).
    async fn locate(&self, args: &Value, kind: Kind) -> Result<CalendarObject> {
        if let Some(u) = s(args, kind.url_key).filter(|u| !u.is_empty()) {
            return self.client.get(self.client.resolve(u)?).await;
        }
        let uid = s(args, "uid").filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("provide {} or uid", kind.url_key))?;
        let cal = self.calendar(args, kind).await?;
        let found = self
            .client
            .find_by_uid(cal.clone(), kind.comp, uid)
            .await?
            .ok_or_else(|| anyhow!("no {} with UID '{uid}' in {cal}", kind.noun))?;
        // Re-fetch to get the authoritative data and etag.
        match self.client.get(found.url.clone()).await {
            Ok(obj) => Ok(obj),
            Err(_) => Ok(found),
        }
    }

    async fn get_event(&self, args: &Value) -> Result<Value> {
        let obj = self.locate(args, EVENT).await?;
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
        let cal = self.calendar(args, EVENT).await?;
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
        let obj = self.locate(args, EVENT).await?;
        let mut root = ical::parse(&obj.data)?;
        let ev = master(&mut root, "VEVENT")?;
        if let Some(sum) = s(args, "summary") {
            ev.set(Property::new("SUMMARY", escape_text(sum)));
        }
        self.apply_fields(ev, args, false)?;
        touch(ev);
        let event_json = ical::event_to_json(ev, self.display_tz(args)?);
        let new_etag = self.put_existing(&obj, args, root.serialize()).await?;
        Ok(json!({ "updated": true, "event_url": obj.url.as_str(), "etag": new_etag, "event": event_json }))
    }

    /// Write back a modified resource, guarded by the caller's etag or the fetched one.
    async fn put_existing(&self, obj: &CalendarObject, args: &Value, data: String) -> Result<Option<String>> {
        let etag = s(args, "etag").map(str::to_string).or(obj.etag.clone());
        let pre = match &etag {
            Some(e) => Precondition::Matches(e),
            None => Precondition::None,
        };
        self.client.put(obj.url.clone(), data, pre).await
    }

    async fn delete(&self, args: &Value, kind: Kind) -> Result<Value> {
        let (url, summary) = if let Some(u) = s(args, kind.url_key).filter(|u| !u.is_empty()) {
            (self.client.resolve(u)?, None)
        } else {
            let obj = self.locate(args, kind).await?;
            let summary = ical::parse(&obj.data)
                .ok()
                .and_then(|r| r.children.iter().find(|c| c.name == kind.comp).and_then(|e| e.text("SUMMARY")));
            (obj.url, summary)
        };
        self.client.delete(url.clone(), s(args, "etag")).await?;
        Ok(json!({ "deleted": true, kind.url_key: url.as_str(), "summary": summary }))
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
        let kind = if item.name == "VTODO" { TASK } else { EVENT };
        let cal = self.calendar(args, kind).await?;
        let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
        // Reuse the existing resource URL if this UID already lives in the calendar.
        let existing = self.client.find_by_uid(cal.clone(), &item.name, &uid).await.ok().flatten();
        let (url, pre) = match existing {
            Some(obj) if overwrite => (obj.url, Precondition::None),
            Some(obj) => bail!("an object with UID '{uid}' already exists at {} (set overwrite=true to replace it)", obj.url),
            None => (cal.join(&format!("{}.ics", resource_name(&uid)))?, if overwrite { Precondition::None } else { Precondition::MustNotExist }),
        };
        let etag = self.client.put(url.clone(), root.serialize(), pre).await?;
        Ok(json!({ "stored": true, "uid": uid, kind.url_key: url.as_str(), "etag": etag }))
    }

    /// Apply optional event fields from tool arguments to a VEVENT.
    fn apply_fields(&self, ev: &mut Component, args: &Value, creating: bool) -> Result<()> {
        let tz = self.tz(args);
        apply_common(ev, args);
        if let Some(v) = s(args, "status") {
            let v = v.to_ascii_uppercase();
            if !["CONFIRMED", "TENTATIVE", "CANCELLED"].contains(&v.as_str()) {
                bail!("status must be CONFIRMED, TENTATIVE or CANCELLED");
            }
            ev.set(Property::new("STATUS", v));
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
        set_reminders(ev, args, false);
        Ok(())
    }

    async fn list_tasks(&self, args: &Value) -> Result<Value> {
        let cal = self.calendar(args, TASK).await?;
        let tz = self.tz(args);
        let display = self.display_tz(args)?;
        let include_completed = args.get("include_completed").and_then(Value::as_bool).unwrap_or(false);
        let bound = |k: &str| -> Result<Option<chrono::DateTime<Utc>>> {
            s(args, k).filter(|v| !v.is_empty()).map(|v| parse_utc_stamp(&ical::to_utc_range_value(v, tz)?)).transpose()
        };
        let (due_after, due_before) = (bound("due_after")?, bound("due_before")?);
        let query = s(args, "query").map(str::to_lowercase).filter(|q| !q.is_empty());
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;

        let objects = self.client.all_tasks(cal.clone()).await?;
        let mut tasks: Vec<((bool, i64, u8, String), Value)> = Vec::new();
        let mut parse_errors = Vec::new();
        for obj in &objects {
            let mut root = match ical::parse(&obj.data) {
                Ok(r) => r,
                Err(e) => {
                    parse_errors.push(format!("{}: {e}", obj.url));
                    continue;
                }
            };
            expand::normalize_fixed_offsets(&mut root, display);
            for todo in root.children.iter().filter(|c| c.name == "VTODO" && c.prop("RECURRENCE-ID").is_none()) {
                let done = ical::task_done(todo);
                if done && !include_completed {
                    continue;
                }
                if due_after.is_some() || due_before.is_some() {
                    let Some(due) = ical::task_due(todo).map(|t| t.instant(display)) else { continue };
                    if due_after.is_some_and(|a| due < a) || due_before.is_some_and(|b| due >= b) {
                        continue;
                    }
                }
                if let Some(q) = &query {
                    let mut hay: Vec<String> = ["SUMMARY", "DESCRIPTION", "LOCATION"].iter().filter_map(|p| todo.text(p)).collect();
                    hay.extend(todo.props_named("CATEGORIES").map(|p| ical::unescape_text(&p.value)));
                    if !hay.join("\n").to_lowercase().contains(q.as_str()) {
                        continue;
                    }
                }
                let mut j = ical::task_to_json(todo, display);
                j["task_url"] = json!(obj.url.as_str());
                if let Some(e) = &obj.etag {
                    j["etag"] = json!(e);
                }
                // Undefined priority (0) sorts after 9.
                let prio = todo.prop("PRIORITY").and_then(|p| p.value.trim().parse::<u8>().ok()).filter(|p| *p > 0).unwrap_or(10);
                let key = (done, ical::due_sort_key(todo, display), prio, todo.text("SUMMARY").unwrap_or_default().to_lowercase());
                tasks.push((key, j));
            }
        }
        tasks.sort_by(|a, b| a.0.cmp(&b.0));
        let total = tasks.len();
        let tasks: Vec<Value> = tasks.into_iter().take(limit).map(|(_, j)| j).collect();
        let mut out = json!({ "calendar": cal.as_str(), "count": tasks.len(), "tasks": tasks });
        if total > limit {
            out["truncated"] = json!(format!("{total} tasks matched; only the first {limit} are shown"));
        }
        if !parse_errors.is_empty() {
            out["parse_errors"] = json!(parse_errors);
        }
        Ok(out)
    }

    async fn get_task(&self, args: &Value) -> Result<Value> {
        let obj = self.locate(args, TASK).await?;
        let mut root = ical::parse(&obj.data)?;
        let display = self.display_tz(args)?;
        expand::normalize_fixed_offsets(&mut root, display);
        let tasks: Vec<Value> = root.children.iter().filter(|c| c.name == "VTODO").map(|t| ical::task_to_json(t, display)).collect();
        if tasks.is_empty() {
            bail!("{} contains no task (VTODO)", obj.url);
        }
        let mut out = json!({ "task_url": obj.url.as_str(), "etag": obj.etag, "tasks": tasks });
        if args.get("include_raw").and_then(Value::as_bool).unwrap_or(true) {
            out["ics"] = json!(obj.data);
        }
        Ok(out)
    }

    async fn create_task(&self, args: &Value) -> Result<Value> {
        let summary = s(args, "summary").filter(|v| !v.trim().is_empty()).ok_or_else(|| anyhow!("summary is required"))?;
        let cal = self.calendar(args, TASK).await?;
        let uid = s(args, "uid").filter(|u| !u.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{}@caldav-mcp", uuid::Uuid::new_v4()));
        let root = self.new_task(&uid, summary, args)?;
        let url = cal.join(&format!("{}.ics", resource_name(&uid)))?;
        let etag = self.client.put(url.clone(), root.serialize(), Precondition::MustNotExist).await?;
        Ok(json!({
            "created": true,
            "task_url": url.as_str(),
            "etag": etag,
            "task": ical::task_to_json(&root.children[0], self.display_tz(args)?),
        }))
    }

    /// Build a VCALENDAR holding a new VTODO.
    fn new_task(&self, uid: &str, summary: &str, args: &Value) -> Result<Component> {
        let now = ical::utc_stamp(Utc::now());
        let mut todo = Component::new("VTODO");
        todo.props.push(Property::new("UID", uid));
        todo.props.push(Property::new("DTSTAMP", now.clone()));
        todo.props.push(Property::new("CREATED", now));
        todo.props.push(Property::new("SUMMARY", escape_text(summary)));
        self.apply_task_fields(&mut todo, args)?;
        if todo.prop("STATUS").is_none() {
            todo.props.push(Property::new("STATUS", "NEEDS-ACTION"));
        }
        let mut root = Component::new("VCALENDAR");
        root.props.push(Property::new("VERSION", "2.0"));
        root.props.push(Property::new("PRODID", "-//caldav_mcp//EN"));
        root.props.push(Property::new("CALSCALE", "GREGORIAN"));
        root.children.push(todo);
        Ok(root)
    }

    async fn update_task(&self, args: &Value) -> Result<Value> {
        let obj = self.locate(args, TASK).await?;
        let mut root = ical::parse(&obj.data)?;
        let todo = master(&mut root, "VTODO")?;
        if let Some(sum) = s(args, "summary") {
            todo.set(Property::new("SUMMARY", escape_text(sum)));
        }
        self.apply_task_fields(todo, args)?;
        touch(todo);
        let task_json = ical::task_to_json(todo, self.display_tz(args)?);
        let recurring_done = todo.prop("RRULE").is_some() && ical::task_done(todo);
        let new_etag = self.put_existing(&obj, args, root.serialize()).await?;
        let mut out = json!({ "updated": true, "task_url": obj.url.as_str(), "etag": new_etag, "task": task_json });
        if recurring_done {
            out["note"] = json!("This task recurs; the status applies to the whole series.");
        }
        Ok(out)
    }

    /// Apply optional task fields from tool arguments to a VTODO.
    fn apply_task_fields(&self, t: &mut Component, args: &Value) -> Result<()> {
        let tz = self.tz(args);
        apply_common(t, args);

        // Start / due.
        for (key, prop) in [("start", "DTSTART"), ("due", "DUE")] {
            let Some(v) = s(args, key) else { continue };
            // DURATION is relative to DTSTART and conflicts with DUE.
            if prop == "DUE" || v.is_empty() {
                t.remove("DURATION");
            }
            if v.is_empty() { t.remove(prop) } else { t.set(ical::dt_property(prop, v, tz)?) }
        }
        if let (Some(st), Some(due)) = (t.prop("DTSTART"), t.prop("DUE"))
            && let (Some(a), Some(b)) = (naive(&st.value), naive(&due.value))
            && st.param("TZID") == due.param("TZID")
            && b.0 < a.0
        {
            bail!("due is before start");
        }

        // Status / completion.
        let status = s(args, "status").map(str::to_ascii_uppercase);
        if let Some(st) = &status
            && !["NEEDS-ACTION", "IN-PROCESS", "COMPLETED", "CANCELLED"].contains(&st.as_str())
        {
            bail!("status must be NEEDS-ACTION, IN-PROCESS, COMPLETED or CANCELLED");
        }
        let completed = args.get("completed").and_then(Value::as_bool);
        match (completed, status.as_deref()) {
            (Some(true), Some(st)) if st != "COMPLETED" => bail!("completed=true conflicts with status {st}"),
            (Some(false), Some("COMPLETED")) => bail!("completed=false conflicts with status COMPLETED"),
            _ => {}
        }
        let target = status.or(match completed {
            Some(true) => Some("COMPLETED".into()),
            Some(false) if ical::task_done(t) => Some("NEEDS-ACTION".into()),
            _ => None,
        });
        if let Some(st) = target {
            if st == "COMPLETED" {
                if t.prop("COMPLETED").is_none() {
                    t.set(Property::new("COMPLETED", ical::utc_stamp(Utc::now())));
                }
                t.set(Property::new("PERCENT-COMPLETE", "100"));
            } else {
                t.remove("COMPLETED");
                if st != "CANCELLED" && t.prop("PERCENT-COMPLETE").is_some_and(|p| p.value.trim() == "100") {
                    t.remove("PERCENT-COMPLETE");
                }
            }
            t.set(Property::new("STATUS", st));
        }
        if let Some(v) = args.get("percent_complete").filter(|v| !v.is_null()) {
            let p = v.as_u64().filter(|p| *p <= 100).ok_or_else(|| anyhow!("percent_complete must be an integer 0-100"))?;
            t.set(Property::new("PERCENT-COMPLETE", p.to_string()));
        }
        if let Some(v) = args.get("priority").filter(|v| !v.is_null()) {
            let p = v.as_u64().filter(|p| *p <= 9).ok_or_else(|| anyhow!("priority must be an integer 0-9"))?;
            if p == 0 { t.remove("PRIORITY") } else { t.set(Property::new("PRIORITY", p.to_string())) }
        }

        if let Some(v) = s(args, "parent_uid") {
            t.props.retain(|p| !(p.name == "RELATED-TO" && p.param("RELTYPE").is_none_or(|r| r.eq_ignore_ascii_case("PARENT"))));
            if !v.is_empty() {
                t.props.push(Property::new("RELATED-TO", v).with_param("RELTYPE", "PARENT"));
            }
        }

        let wants_reminders = args.get("reminder_minutes").and_then(Value::as_array).is_some_and(|l| !l.is_empty());
        if wants_reminders && t.prop("DUE").is_none() && t.prop("DTSTART").is_none() {
            bail!("reminder_minutes needs a due or start time");
        }
        let relative_to_due = t.prop("DUE").is_some();
        set_reminders(t, args, relative_to_due);
        Ok(())
    }
}

/// Description, location, URL, recurrence rule and categories (events and tasks).
fn apply_common(c: &mut Component, args: &Value) {
    for (key, prop) in [("description", "DESCRIPTION"), ("location", "LOCATION")] {
        if let Some(v) = s(args, key) {
            if v.is_empty() { c.remove(prop) } else { c.set(Property::new(prop, escape_text(v))) }
        }
    }
    if let Some(v) = s(args, "url") {
        if v.is_empty() { c.remove("URL") } else { c.set(Property::new("URL", v)) }
    }
    if let Some(v) = s(args, "rrule") {
        let v = v.trim().trim_start_matches("RRULE:");
        if v.is_empty() { c.remove("RRULE") } else { c.set(Property::new("RRULE", v)) }
    }
    if let Some(list) = args.get("categories").and_then(Value::as_array) {
        c.remove("CATEGORIES");
        let cats: Vec<String> = list.iter().filter_map(Value::as_str).map(escape_text).collect();
        if !cats.is_empty() {
            c.props.push(Property::new("CATEGORIES", cats.join(",")));
        }
    }
}

/// Replace display alarms from `reminder_minutes`. With `relative_to_end`, triggers are
/// relative to DTEND/DUE (RELATED=END) instead of DTSTART.
fn set_reminders(c: &mut Component, args: &Value, relative_to_end: bool) {
    let Some(list) = args.get("reminder_minutes").and_then(Value::as_array) else { return };
    c.children.retain(|a| a.name != "VALARM");
    let title = c.text("SUMMARY").unwrap_or_else(|| "Reminder".into());
    for m in list.iter().filter_map(Value::as_u64) {
        let mut alarm = Component::new("VALARM");
        alarm.props.push(Property::new("ACTION", "DISPLAY"));
        alarm.props.push(Property::new("DESCRIPTION", escape_text(&title)));
        let trigger = Property::new("TRIGGER", format!("-PT{m}M"));
        alarm.props.push(if relative_to_end { trigger.with_param("RELATED", "END") } else { trigger });
        c.children.push(alarm);
    }
}

/// Bump SEQUENCE and the modification stamps after an edit.
fn touch(c: &mut Component) {
    let seq = c.prop("SEQUENCE").and_then(|p| p.value.trim().parse::<i64>().ok()).unwrap_or(0);
    c.set(Property::new("SEQUENCE", (seq + 1).to_string()));
    let now = ical::utc_stamp(Utc::now());
    c.set(Property::new("DTSTAMP", now.clone()));
    c.set(Property::new("LAST-MODIFIED", now));
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

/// The master component of this type (no RECURRENCE-ID), or the first one.
fn master<'a>(root: &'a mut Component, comp: &str) -> Result<&'a mut Component> {
    let idx = root
        .children
        .iter()
        .position(|c| c.name == comp && c.prop("RECURRENCE-ID").is_none())
        .or_else(|| root.children.iter().position(|c| c.name == comp))
        .ok_or_else(|| anyhow!("resource contains no {comp}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caldav::{Auth, Config};

    fn tools() -> Tools {
        Tools {
            client: CalDavClient::new(Config { base_url: Url::parse("http://localhost/").unwrap(), auth: Auth::None }).unwrap(),
            default_calendar: None,
            default_task_list: None,
            default_timezone: Some("Europe/Berlin".into()),
        }
    }

    #[test]
    fn create_complete_reopen_task() {
        let t = tools();
        let args = json!({"due": "2026-11-20T17:00", "priority": 1, "reminder_minutes": [30], "parent_uid": "p1", "categories": ["home"]});
        let mut root = t.new_task("u1", "Pay rent", &args).unwrap();
        let todo = &mut root.children[0];
        assert_eq!(todo.prop("DUE").unwrap().param("TZID"), Some("Europe/Berlin"));
        assert_eq!(todo.prop("STATUS").unwrap().value, "NEEDS-ACTION");
        assert_eq!(todo.children[0].prop("TRIGGER").unwrap().param("RELATED"), Some("END"));
        let j = ical::task_to_json(todo, Some(chrono_tz::Europe::Berlin));
        assert_eq!(j["due"], "2026-11-20T17:00:00+01:00");
        assert_eq!(j["priority"], 1);
        assert_eq!(j["parent_uid"], "p1");
        assert_eq!(j["completed"], false);

        t.apply_task_fields(todo, &json!({"completed": true})).unwrap();
        assert!(ical::task_done(todo));
        assert_eq!(todo.prop("PERCENT-COMPLETE").unwrap().value, "100");
        assert!(todo.prop("COMPLETED").is_some());
        assert_eq!(ical::task_to_json(todo, None)["completed"], true);

        t.apply_task_fields(todo, &json!({"completed": false})).unwrap();
        assert!(!ical::task_done(todo));
        assert!(todo.prop("COMPLETED").is_none() && todo.prop("PERCENT-COMPLETE").is_none());
        assert_eq!(todo.prop("STATUS").unwrap().value, "NEEDS-ACTION");

        t.apply_task_fields(todo, &json!({"due": "", "priority": 0})).unwrap();
        assert!(todo.prop("DUE").is_none() && todo.prop("PRIORITY").is_none());
    }

    #[test]
    fn task_validation() {
        let t = tools();
        assert!(t.new_task("u", "x", &json!({"start": "2026-11-20", "due": "2026-11-19"})).is_err());
        assert!(t.new_task("u", "x", &json!({"completed": true, "status": "IN-PROCESS"})).is_err());
        assert!(t.new_task("u", "x", &json!({"reminder_minutes": [10]})).is_err());
        assert!(t.new_task("u", "x", &json!({"priority": 12})).is_err());
        let root = t.new_task("u", "x", &json!({"start": "2026-11-20T09:00", "reminder_minutes": [5]})).unwrap();
        assert_eq!(root.children[0].children[0].prop("TRIGGER").unwrap().param("RELATED"), None);
    }

    #[test]
    fn task_due_from_duration() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:d\r\nDTSTART:20261120T080000Z\r\nDURATION:PT2H\r\nSTATUS:IN-PROCESS\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let root = ical::parse(ics).unwrap();
        let j = ical::task_to_json(&root.children[0], None);
        assert_eq!(j["due"], "2026-11-20T10:00:00Z");
        assert_eq!(j["completed"], false);
        assert!(j.get("duration").is_none());
    }
}
