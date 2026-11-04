//! Client-side recurrence expansion and timezone normalisation.
//!
//! Server-side expansion (CalDAV `<C:expand>`) is not used because servers mishandle
//! non-IANA TZIDs: Nextcloud/sabre treats e.g. `TZID=UTC+02:00` as UTC, shifting events.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use rrule::{RRule, RRuleSet, Unvalidated};

use crate::ical::{Component, IcalTime, Property, parse_duration, parse_fixed_offset, parse_tz};

const TIME_PROPS: [&str; 5] = ["DTSTART", "DTEND", "RECURRENCE-ID", "EXDATE", "RDATE"];
/// Upper bound on generated occurrences per event, to keep runaway rules cheap.
const MAX_OCCURRENCES: usize = 5000;

/// Rewrite fixed-offset TZIDs such as `UTC+02:00` (written by sync bridges that lost the
/// real zone). When the offset is one `display` uses (standard or daylight time), the value
/// is relabelled with that zone, keeping its wall-clock time so recurrences follow DST.
/// Otherwise it is converted to UTC.
pub fn normalize_fixed_offsets(root: &mut Component, display: Option<Tz>) {
    for comp in &mut root.children {
        for p in comp.props.iter_mut().filter(|p| TIME_PROPS.contains(&p.name.as_str())) {
            let Some(off) = p.param("TZID").filter(|id| parse_tz(id).is_none()).and_then(parse_fixed_offset) else {
                continue;
            };
            let values: Vec<NaiveDateTime> = p
                .value
                .split(',')
                .filter_map(|v| NaiveDateTime::parse_from_str(v.trim(), "%Y%m%dT%H%M%S").ok())
                .collect();
            let Some(first) = values.first() else { continue };
            let relabel = display.filter(|tz| {
                [1, 7].iter().any(|&month| {
                    let probe = NaiveDate::from_ymd_opt(first.year(), month, 1).unwrap().and_hms_opt(12, 0, 0).unwrap();
                    tz.offset_from_utc_datetime(&probe).fix() == off
                })
            });
            p.params.retain(|(k, _)| k != "TZID");
            p.value = match relabel {
                Some(tz) => {
                    p.params.push(("TZID".into(), tz.name().into()));
                    values.iter().map(|v| v.format("%Y%m%dT%H%M%S").to_string()).collect::<Vec<_>>().join(",")
                }
                None => values
                    .iter()
                    .map(|v| (*v - Duration::seconds(off.local_minus_utc() as i64)).format("%Y%m%dT%H%M%SZ").to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            };
        }
    }
}

/// Expand the VEVENTs of one calendar object into the occurrences overlapping
/// `[start, end)`. Overridden occurrences (RECURRENCE-ID) replace generated ones.
/// Returns the occurrence components and any warnings.
pub fn expand(root: &Component, start: DateTime<Utc>, end: DateTime<Utc>, display: Option<Tz>) -> (Vec<Component>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let vevents: Vec<&Component> = root.children.iter().filter(|c| c.name == "VEVENT").collect();
    let overrides: Vec<&Component> = vevents.iter().copied().filter(|c| c.prop("RECURRENCE-ID").is_some()).collect();
    let overridden: Vec<DateTime<Utc>> = overrides
        .iter()
        .filter_map(|o| o.prop("RECURRENCE-ID").and_then(IcalTime::from_property))
        .map(|t| t.instant(display))
        .collect();

    for master in vevents.iter().filter(|c| c.prop("RECURRENCE-ID").is_none()) {
        let Some(st) = master.prop("DTSTART").and_then(IcalTime::from_property) else { continue };
        let dur = event_duration(master, &st, display);
        let recurring = master.prop("RRULE").is_some() || master.prop("RDATE").is_some();
        if !recurring {
            if overlaps(st.instant(display), dur, start, end) {
                out.push((*master).clone());
            }
            continue;
        }
        match occurrences(master, &st, dur, start, end, display) {
            Ok(occ) => {
                for o in occ {
                    if overridden.contains(&o.with_timezone(&Utc)) {
                        continue;
                    }
                    out.push(instance(master, &st, o, dur));
                }
            }
            Err(e) => {
                warnings.push(format!("{}: cannot expand recurrence ({e}); returned the series master instead", master.text("SUMMARY").unwrap_or_default()));
                out.push((*master).clone());
            }
        }
    }
    for o in overrides {
        if let Some(st) = o.prop("DTSTART").and_then(IcalTime::from_property)
            && overlaps(st.instant(display), event_duration(o, &st, display), start, end)
        {
            out.push(o.clone());
        }
    }
    (out, warnings)
}

fn overlaps(ev_start: DateTime<Utc>, dur: Duration, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    ev_start < end && (ev_start + dur > start || (dur.is_zero() && ev_start >= start))
}

fn event_duration(ev: &Component, st: &IcalTime, display: Option<Tz>) -> Duration {
    if let Some(en) = ev.prop("DTEND").and_then(IcalTime::from_property) {
        return en.instant(display) - st.instant(display);
    }
    if let Some(d) = ev.prop("DURATION").and_then(|p| parse_duration(&p.value)) {
        return d;
    }
    if matches!(st, IcalTime::Date(_)) { Duration::days(1) } else { Duration::zero() }
}

/// The zone recurrences are computed in, and the start's wall-clock time in it.
fn working_zone(st: &IcalTime, display: Option<Tz>) -> (Tz, NaiveDateTime) {
    let fallback = display.unwrap_or(chrono_tz::UTC);
    match st {
        IcalTime::Date(d) => (fallback, d.and_hms_opt(0, 0, 0).unwrap()),
        IcalTime::Utc(x) => (chrono_tz::UTC, x.naive_utc()),
        IcalTime::Zoned(n, tz) => (*tz, *n),
        IcalTime::Fixed(..) => (chrono_tz::UTC, st.instant(None).naive_utc()),
        IcalTime::Floating(n) | IcalTime::UnknownZone(n, _) => (fallback, *n),
    }
}

fn occurrences(
    master: &Component,
    st: &IcalTime,
    dur: Duration,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    display: Option<Tz>,
) -> Result<Vec<DateTime<rrule::Tz>>, String> {
    let (zone, naive) = working_zone(st, display);
    let rz = rrule::Tz::Tz(zone);
    let local = |n: NaiveDateTime| -> DateTime<rrule::Tz> {
        rz.from_local_datetime(&n)
            .earliest()
            .unwrap_or_else(|| rz.from_local_datetime(&(n + Duration::hours(1))).earliest().unwrap_or_else(|| rz.from_utc_datetime(&n)))
    };
    let dtstart = local(naive);
    let mut set = RRuleSet::new(dtstart);
    let mut until: Option<DateTime<Utc>> = None;

    for p in master.props_named("RRULE") {
        // UNTIL is applied below: its value forms vary (date, UTC, local) and the
        // library rejects combinations that are common in the wild.
        let mut parts = Vec::new();
        for part in p.value.split(';').filter(|s| !s.is_empty()) {
            match part.split_once('=') {
                Some((k, v)) if k.eq_ignore_ascii_case("UNTIL") => {
                    let u = until_instant(v, &zone);
                    until = Some(until.map_or(u, |x: DateTime<Utc>| x.max(u)));
                }
                _ => parts.push(part),
            }
        }
        let rule: RRule<Unvalidated> = parts.join(";").parse().map_err(|e| format!("{e}"))?;
        set = set.rrule(rule.validate(dtstart).map_err(|e| format!("{e}"))?);
    }
    for (name, is_ex) in [("RDATE", false), ("EXDATE", true)] {
        for p in master.props_named(name) {
            for v in p.value.split(',').filter(|v| !v.contains('/')) {
                let single = Property { name: p.name.clone(), params: p.params.clone(), value: v.trim().to_string() };
                let Some(t) = IcalTime::from_property(&single) else { continue };
                let dt = match t {
                    IcalTime::Date(d) => local(d.and_hms_opt(naive.hour(), naive.minute(), 0).unwrap()),
                    IcalTime::Floating(n) | IcalTime::UnknownZone(n, _) => local(n),
                    other => other.instant(display).with_timezone(&rz),
                };
                set = if is_ex { set.exdate(dt) } else { set.rdate(dt) };
            }
        }
    }

    let mut result = Vec::new();
    for (i, occ) in set.into_iter().enumerate() {
        let u = occ.with_timezone(&Utc);
        if i >= MAX_OCCURRENCES || u >= end || until.is_some_and(|x| u > x) {
            break;
        }
        if overlaps(u, dur, start, end) {
            result.push(occ);
        }
    }
    Ok(result)
}

fn until_instant(v: &str, zone: &Tz) -> DateTime<Utc> {
    let v = v.trim();
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y%m%d") {
        let n = d.and_hms_opt(23, 59, 59).unwrap();
        return zone.from_local_datetime(&n).earliest().map(|x| x.with_timezone(&Utc)).unwrap_or(n.and_utc());
    }
    if let Some(z) = v.strip_suffix('Z')
        && let Ok(n) = NaiveDateTime::parse_from_str(z, "%Y%m%dT%H%M%S")
    {
        return n.and_utc();
    }
    match NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S") {
        Ok(n) => zone.from_local_datetime(&n).earliest().map(|x| x.with_timezone(&Utc)).unwrap_or(n.and_utc()),
        Err(_) => DateTime::<Utc>::MAX_UTC,
    }
}

/// A concrete occurrence of `master` starting at `occ`, written in the same style
/// (all-day / zoned / UTC / floating) as the master's DTSTART.
fn instance(master: &Component, st: &IcalTime, occ: DateTime<rrule::Tz>, dur: Duration) -> Component {
    let mut ev = master.clone();
    for p in ["RRULE", "RDATE", "EXDATE", "EXRULE", "DURATION", "DTEND"] {
        ev.remove(p);
    }
    let start_prop = time_prop("DTSTART", st, occ.naive_local());
    let end_naive = match st {
        IcalTime::Date(_) => occ.naive_local() + Duration::days(dur.num_days()),
        _ => (occ + dur).naive_local(),
    };
    let mut rid = start_prop.clone();
    rid.name = "RECURRENCE-ID".into();
    ev.set(start_prop);
    ev.props.push(time_prop("DTEND", st, end_naive));
    ev.props.push(rid);
    ev
}

fn time_prop(name: &str, style: &IcalTime, n: NaiveDateTime) -> Property {
    match style {
        IcalTime::Date(_) => Property::new(name, n.date().format("%Y%m%d").to_string()).with_param("VALUE", "DATE"),
        IcalTime::Utc(_) | IcalTime::Fixed(..) => Property::new(name, n.format("%Y%m%dT%H%M%SZ").to_string()),
        IcalTime::Zoned(_, tz) => Property::new(name, n.format("%Y%m%dT%H%M%S").to_string()).with_param("TZID", tz.name()),
        IcalTime::UnknownZone(_, id) => Property::new(name, n.format("%Y%m%dT%H%M%S").to_string()).with_param("TZID", id),
        IcalTime::Floating(_) => Property::new(name, n.format("%Y%m%dT%H%M%S").to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ical::{event_to_json, parse};

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    const BERLIN: Option<Tz> = Some(chrono_tz::Europe::Berlin);

    #[test]
    fn fixed_offset_weekly_follows_dst() {
        // As written by the Nextcloud Google bridge: offset label instead of a real zone.
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Salsa\r\nDTSTART;TZID=\"UTC+02:00\";VALUE=DATE-TIME:20261021T173000\r\nDTEND;TZID=\"UTC+02:00\";VALUE=DATE-TIME:20261021T190000\r\nRRULE:FREQ=WEEKLY;UNTIL=20261231T235959Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let mut root = parse(ics).unwrap();
        normalize_fixed_offsets(&mut root, BERLIN);
        let (occ, warn) = expand(&root, utc("2026-10-20T00:00:00Z"), utc("2026-11-05T00:00:00Z"), BERLIN);
        assert!(warn.is_empty());
        let starts: Vec<String> = occ.iter().map(|e| event_to_json(e, BERLIN)["start"].as_str().unwrap().to_string()).collect();
        assert_eq!(starts, ["2026-10-21T17:30:00+02:00", "2026-10-28T17:30:00+01:00", "2026-11-04T17:30:00+01:00"]);
        assert_eq!(event_to_json(&occ[1], BERLIN)["end"], "2026-10-28T19:00:00+01:00");
    }

    #[test]
    fn foreign_fixed_offset_converted_to_utc() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:b\r\nDTSTART;TZID=\"UTC+05:30\":20261021T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let mut root = parse(ics).unwrap();
        normalize_fixed_offsets(&mut root, BERLIN);
        assert_eq!(root.children[0].prop("DTSTART").unwrap().value, "20261021T043000Z");
    }

    #[test]
    fn overrides_exdates_and_all_day() {
        let ics = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\nUID:c\r\nDTSTART;TZID=Europe/Berlin:20261102T090000\r\nDURATION:PT30M\r\nRRULE:FREQ=DAILY;COUNT=5\r\nEXDATE;TZID=Europe/Berlin:20261103T090000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:c\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261104T090000\r\nDTSTART;TZID=Europe/Berlin:20261104T150000\r\nDTEND;TZID=Europe/Berlin:20261104T153000\r\nSUMMARY:moved\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";
        let root = parse(ics).unwrap();
        let (occ, _) = expand(&root, utc("2026-11-01T00:00:00Z"), utc("2026-12-01T00:00:00Z"), BERLIN);
        let mut starts: Vec<String> = occ.iter().map(|e| event_to_json(e, BERLIN)["start"].as_str().unwrap().to_string()).collect();
        starts.sort();
        assert_eq!(
            starts,
            ["2026-11-02T09:00:00+01:00", "2026-11-04T15:00:00+01:00", "2026-11-05T09:00:00+01:00", "2026-11-06T09:00:00+01:00"]
        );

        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:d\r\nDTSTART;VALUE=DATE:20260101\r\nRRULE:FREQ=YEARLY;UNTIL=20300101\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let (occ, _) = expand(&parse(ics).unwrap(), utc("2026-12-31T00:00:00Z"), utc("2028-01-02T00:00:00Z"), BERLIN);
        let days: Vec<_> = occ.iter().map(|e| event_to_json(e, BERLIN)["start"].clone()).collect();
        assert_eq!(days, ["2027-01-01", "2028-01-01"]);
        assert_eq!(event_to_json(&occ[0], BERLIN)["end"], "2027-01-02");
    }
}
