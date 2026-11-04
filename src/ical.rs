//! Minimal iCalendar (RFC 5545) parser/serializer, sufficient for reading and
//! editing VEVENTs without losing unknown properties or components.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl Property {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Self { name: name.to_ascii_uppercase(), params: Vec::new(), value: value.into() }
    }

    pub fn with_param(mut self, name: &str, value: &str) -> Self {
        self.params.push((name.to_ascii_uppercase(), value.to_string()));
        self
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    pub name: String,
    pub props: Vec<Property>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_ascii_uppercase(), props: Vec::new(), children: Vec::new() }
    }

    pub fn prop(&self, name: &str) -> Option<&Property> {
        self.props.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.props.iter().filter(move |p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn remove(&mut self, name: &str) {
        self.props.retain(|p| !p.name.eq_ignore_ascii_case(name));
    }

    /// Replace all properties with this name by `prop` (keeping the position of the first one).
    pub fn set(&mut self, prop: Property) {
        match self.props.iter().position(|p| p.name.eq_ignore_ascii_case(&prop.name)) {
            Some(i) => {
                let name = prop.name.clone();
                self.props[i] = prop;
                let mut seen = false;
                self.props.retain(|p| {
                    if p.name.eq_ignore_ascii_case(&name) {
                        let keep = !seen;
                        seen = true;
                        keep
                    } else {
                        true
                    }
                });
            }
            None => self.props.push(prop),
        }
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.prop(name).map(|p| unescape_text(&p.value))
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        push_folded(out, &format!("BEGIN:{}", self.name));
        for p in &self.props {
            let mut line = p.name.clone();
            for (k, v) in &p.params {
                line.push(';');
                line.push_str(k);
                line.push('=');
                if v.contains([':', ';', ',']) {
                    line.push('"');
                    line.push_str(v);
                    line.push('"');
                } else {
                    line.push_str(v);
                }
            }
            line.push(':');
            line.push_str(&p.value);
            push_folded(out, &line);
        }
        for c in &self.children {
            c.write_to(out);
        }
        push_folded(out, &format!("END:{}", self.name));
    }
}

/// Fold a content line at 75 octets (RFC 5545 §3.1), respecting UTF-8 boundaries.
fn push_folded(out: &mut String, line: &str) {
    let mut count = 0;
    for ch in line.chars() {
        let len = ch.len_utf8();
        if count + len > 75 {
            out.push_str("\r\n ");
            count = 1;
        }
        out.push(ch);
        count += len;
    }
    out.push_str("\r\n");
}

/// Parse an iCalendar stream and return its root component (normally VCALENDAR).
pub fn parse(text: &str) -> Result<Component> {
    // Unfold lines.
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        if !raw.is_empty() {
            lines.push(raw.to_string());
        }
    }

    let mut stack: Vec<Component> = Vec::new();
    let mut root: Option<Component> = None;
    for line in &lines {
        let prop = parse_line(line).with_context(|| format!("invalid iCalendar line: {line}"))?;
        match prop.name.as_str() {
            "BEGIN" => stack.push(Component::new(&prop.value)),
            "END" => {
                let comp = stack.pop().ok_or_else(|| anyhow!("unbalanced END:{}", prop.value))?;
                if !comp.name.eq_ignore_ascii_case(&prop.value) {
                    bail!("END:{} does not match BEGIN:{}", prop.value, comp.name);
                }
                match stack.last_mut() {
                    Some(parent) => parent.children.push(comp),
                    None => {
                        root = Some(comp);
                        break;
                    }
                }
            }
            _ => match stack.last_mut() {
                Some(c) => c.props.push(prop),
                None => bail!("property outside of a component: {line}"),
            },
        }
    }
    root.ok_or_else(|| anyhow!("no complete iCalendar component found"))
}

fn parse_line(line: &str) -> Result<Property> {
    // name *(";" param) ":" value — param values may be quoted and contain ':' or ';'.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] != b';' && bytes[i] != b':' {
        i += 1;
    }
    if i >= bytes.len() {
        bail!("missing ':'");
    }
    let name = line[..i].trim().to_ascii_uppercase();
    let mut params = Vec::new();
    while bytes[i] == b';' {
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            bail!("malformed parameter");
        }
        let pname = line[start..i].to_ascii_uppercase();
        i += 1;
        let mut pval = String::new();
        let mut in_quotes = false;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'"' {
                in_quotes = !in_quotes;
            } else if !in_quotes && (b == b';' || b == b':') {
                break;
            } else {
                // push whole UTF-8 char
                let ch = line[i..].chars().next().unwrap();
                pval.push(ch);
                i += ch.len_utf8();
                continue;
            }
            i += 1;
        }
        if i >= bytes.len() {
            bail!("missing ':'");
        }
        params.push((pname, pval));
    }
    Ok(Property { name, params, value: line[i + 1..].to_string() })
}

pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

pub fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn utc_stamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// A date or date-time value as understood from user input.
pub enum DtInput {
    Date(NaiveDate),
    Utc(DateTime<Utc>),
    Local(NaiveDateTime),
}

pub fn parse_dt_input(s: &str) -> Result<DtInput> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(DtInput::Date(d));
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(DtInput::Utc(dt.with_timezone(&Utc)));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(DtInput::Local(dt));
        }
    }
    bail!(
        "cannot parse date/time '{s}'; use YYYY-MM-DD, YYYY-MM-DDTHH:MM[:SS] or RFC 3339 (e.g. 2026-11-05T10:00:00+01:00)"
    )
}

/// Build a DTSTART/DTEND style property from user input.
/// Naive local times use `timezone` (IANA name) as TZID, or are floating if none is given.
pub fn dt_property(name: &str, input: &str, timezone: Option<&str>) -> Result<Property> {
    Ok(match parse_dt_input(input)? {
        DtInput::Date(d) => Property::new(name, d.format("%Y%m%d").to_string()).with_param("VALUE", "DATE"),
        DtInput::Utc(dt) => Property::new(name, utc_stamp(dt)),
        DtInput::Local(dt) => {
            let p = Property::new(name, dt.format("%Y%m%dT%H%M%S").to_string());
            match timezone {
                Some(tz) => {
                    tz.parse::<chrono_tz::Tz>()
                        .map_err(|_| anyhow!("unknown IANA timezone '{tz}'"))?;
                    p.with_param("TZID", tz)
                }
                None => p,
            }
        }
    })
}

/// Convert user input to the UTC form required by CalDAV time-range filters.
pub fn to_utc_range_value(input: &str, timezone: Option<&str>) -> Result<String> {
    let tz: Option<chrono_tz::Tz> = match timezone {
        Some(t) => Some(t.parse().map_err(|_| anyhow!("unknown IANA timezone '{t}'"))?),
        None => None,
    };
    let local_to_utc = |dt: NaiveDateTime| -> Result<DateTime<Utc>> {
        Ok(match tz {
            Some(tz) => dt
                .and_local_timezone(tz)
                .earliest()
                .ok_or_else(|| anyhow!("invalid local time {dt} in {}", tz.name()))?
                .with_timezone(&Utc),
            None => dt.and_utc(),
        })
    };
    let utc = match parse_dt_input(input)? {
        DtInput::Date(d) => local_to_utc(d.and_hms_opt(0, 0, 0).unwrap())?,
        DtInput::Utc(dt) => dt,
        DtInput::Local(dt) => local_to_utc(dt)?,
    };
    Ok(utc_stamp(utc))
}

/// Render an iCalendar DATE / DATE-TIME value in ISO 8601 form.
pub fn format_dt_value(value: &str) -> String {
    let v = value.trim();
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y%m%d") {
        return d.format("%Y-%m-%d").to_string();
    }
    if let Some(stripped) = v.strip_suffix('Z')
        && let Ok(dt) = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S") {
            return dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        }
    if let Ok(dt) = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S") {
        return dt.format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    v.to_string()
}

/// Convert a VEVENT into a JSON object for tool output.
pub fn event_to_json(ev: &Component) -> Value {
    let mut m = Map::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v
            && !v.is_empty() {
                m.insert(k.to_string(), Value::String(v));
            }
    };
    put("uid", ev.prop("UID").map(|p| p.value.clone()));
    put("summary", ev.text("SUMMARY"));
    put("description", ev.text("DESCRIPTION"));
    put("location", ev.text("LOCATION"));
    put("status", ev.prop("STATUS").map(|p| p.value.clone()));
    put("rrule", ev.prop("RRULE").map(|p| p.value.clone()));
    put("recurrence_id", ev.prop("RECURRENCE-ID").map(|p| format_dt_value(&p.value)));
    put("duration", ev.prop("DURATION").map(|p| p.value.clone()));
    put("url", ev.prop("URL").map(|p| p.value.clone()));
    put("organizer", ev.prop("ORGANIZER").map(|p| strip_mailto(&p.value)));
    for (key, name) in [("start", "DTSTART"), ("end", "DTEND")] {
        if let Some(p) = ev.prop(name) {
            m.insert(key.to_string(), Value::String(format_dt_value(&p.value)));
            if let Some(tz) = p.param("TZID") {
                m.insert(format!("{key}_timezone"), Value::String(tz.to_string()));
            }
        }
    }
    let all_day = ev
        .prop("DTSTART")
        .map(|p| p.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || p.value.trim().len() == 8)
        .unwrap_or(false);
    m.insert("all_day".into(), Value::Bool(all_day));
    let attendees: Vec<Value> = ev
        .props_named("ATTENDEE")
        .map(|p| {
            let mut a = json!({ "email": strip_mailto(&p.value) });
            if let Some(cn) = p.param("CN") {
                a["name"] = json!(cn);
            }
            if let Some(ps) = p.param("PARTSTAT") {
                a["status"] = json!(ps);
            }
            a
        })
        .collect();
    if !attendees.is_empty() {
        m.insert("attendees".into(), Value::Array(attendees));
    }
    let categories: Vec<String> = ev
        .props_named("CATEGORIES")
        .flat_map(|p| split_text_list(&p.value))
        .collect();
    if !categories.is_empty() {
        m.insert("categories".into(), json!(categories));
    }
    let reminders: Vec<String> = ev
        .children
        .iter()
        .filter(|c| c.name == "VALARM")
        .filter_map(|c| c.prop("TRIGGER").map(|p| p.value.clone()))
        .collect();
    if !reminders.is_empty() {
        m.insert("reminders".into(), json!(reminders));
    }
    Value::Object(m)
}

fn split_text_list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push('\\');
                    cur.push(n);
                }
            }
            ',' => out.push(unescape_text(&std::mem::take(&mut cur))),
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(unescape_text(&cur));
    }
    out
}

fn strip_mailto(v: &str) -> String {
    let lower = v.to_ascii_lowercase();
    if lower.starts_with("mailto:") { v[7..].to_string() } else { v.to_string() }
}

/// Sort key approximating chronological order of a VEVENT's start.
pub fn start_sort_key(ev: &Component) -> String {
    ev.prop("DTSTART").map(|p| p.value.trim_end_matches('Z').to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//y//EN\r\nBEGIN:VEVENT\r\nUID:abc@example.com\r\nDTSTART;TZID=Europe/Berlin:20261105T100000\r\nDTEND;TZID=Europe/Berlin:20261105T110000\r\nSUMMARY:Team\\, sync\r\nDESCRIPTION:Line one\\nline two that is very long and needs to be folded becau\r\n se it exceeds seventy-five octets\r\nATTENDEE;CN=\"Doe, Jane\";PARTSTAT=ACCEPTED:mailto:jane@example.com\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn roundtrip() {
        let cal = parse(SAMPLE).unwrap();
        assert_eq!(cal.name, "VCALENDAR");
        let ev = &cal.children[0];
        assert_eq!(ev.text("SUMMARY").unwrap(), "Team, sync");
        assert!(ev.text("DESCRIPTION").unwrap().contains("because it exceeds"));
        assert_eq!(ev.prop("ATTENDEE").unwrap().param("CN"), Some("Doe, Jane"));
        let out = cal.serialize();
        assert!(out.lines().all(|l| l.len() <= 76));
        assert_eq!(parse(&out).unwrap(), cal);
        let j = event_to_json(ev);
        assert_eq!(j["start"], "2026-11-05T10:00:00");
        assert_eq!(j["start_timezone"], "Europe/Berlin");
        assert_eq!(j["reminders"][0], "-PT15M");
    }

    #[test]
    fn dt_inputs() {
        assert_eq!(dt_property("DTSTART", "2026-11-05", None).unwrap().value, "20261105");
        assert_eq!(dt_property("DTSTART", "2026-11-05T10:00:00+01:00", None).unwrap().value, "20261105T090000Z");
        let p = dt_property("DTSTART", "2026-11-05T10:00", Some("Europe/Berlin")).unwrap();
        assert_eq!(p.param("TZID"), Some("Europe/Berlin"));
        assert_eq!(to_utc_range_value("2026-11-05T10:00", Some("Europe/Berlin")).unwrap(), "20261105T090000Z");
        assert!(dt_property("DTSTART", "2026-11-05T10:00", Some("Mars/Base")).is_err());
    }
}
