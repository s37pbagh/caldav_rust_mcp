# caldav_mcp

An [MCP](https://modelcontextprotocol.io) server (stdio) that gives an AI assistant read/write access to the events and tasks (to-dos) in any CalDAV calendar: Nextcloud, iCloud, Fastmail, Radicale, Baïkal, SOGo, Google and others.

## Tools

| Tool | What it does |
|---|---|
| `list_calendars` | Lists calendars (URL, name, color, supported components) |
| `list_events` | Lists events in a time range (default: next 7 days). Recurring events are expanded into individual occurrences by default. Optional text filter. |
| `get_event` | Fetches one event by `event_url` or `uid` (+ `calendar`), including the raw ICS |
| `create_event` | Creates an event: summary, start/end, all-day, timezone, location, description, recurrence rule, attendees, organizer, categories, reminders, URL |
| `update_event` | Changes only the fields you pass and keeps everything else. When you move the start, the event keeps its original length. Uses an ETag check so it won't overwrite someone else's changes. |
| `delete_event` | Deletes an event (optional ETag check) |
| `list_tasks` | Lists tasks (VTODO). Only open tasks by default (`include_completed` adds done/cancelled ones). Sorted by due date, then priority. Optional `due_after` / `due_before` and text filter. |
| `get_task` | Fetches one task by `task_url` or `uid` (+ `calendar`), including the raw ICS |
| `create_task` | Creates a task: summary, due, start, timezone, description, location, priority (1–9), status, percent complete, parent task (subtasks), categories, reminders, recurrence rule, URL |
| `update_task` | Changes only the fields you pass. `completed: true` marks it done (status, completion time, 100%); `completed: false` reopens it. Uses an ETag check. |
| `delete_task` | Deletes a task (optional ETag check) |
| `import_ics` | Stores raw iCalendar data containing an event or a task (`overwrite` lets it replace an existing one) |

You can give `calendar` as a URL from `list_calendars` or as a display name (e.g. `"Work"`).

Date/time formats: `2026-11-05` (all-day), `2026-11-05T10:00` (local time, uses `timezone`), `2026-11-05T10:00:00+01:00` / `...Z` (absolute).

## Configuration

| Variable | |
|---|---|
| `CALDAV_URL` | **Required.** Server root, principal URL or calendar home. The calendars are discovered from it (also via `/.well-known/caldav`). |
| `CALDAV_USERNAME` / `CALDAV_PASSWORD` | HTTP Basic auth (use an app password for iCloud/Fastmail/Nextcloud with 2FA) |
| `CALDAV_TOKEN` | Bearer token (alternative to Basic auth) |
| `CALDAV_DEFAULT_CALENDAR` | Default calendar name or URL |
| `CALDAV_DEFAULT_TASK_LIST` | Default calendar for tasks. Without it, tasks go to `CALDAV_DEFAULT_CALENDAR` if that calendar supports tasks, otherwise to the first calendar that does. |
| `CALDAV_TIMEZONE` | Default IANA timezone for local times, e.g. `Europe/Berlin` |

Example URLs:
- Nextcloud: `https://cloud.example.com/remote.php/dav`
- iCloud: `https://caldav.icloud.com`
- Fastmail: `https://caldav.fastmail.com/dav/`
- Radicale: `http://localhost:5232/`

## Build & register

```sh
cargo build --release
```

Claude Code:

```sh
claude mcp add caldav \
  -e CALDAV_URL=https://cloud.example.com/remote.php/dav \
  -e CALDAV_USERNAME=me -e CALDAV_PASSWORD=app-password \
  -e CALDAV_TIMEZONE=Europe/Berlin \
  -- /path/to/caldav_mcp/target/release/caldav_mcp
```

Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "caldav": {
      "command": "/path/to/caldav_mcp/target/release/caldav_mcp",
      "env": { "CALDAV_URL": "https://caldav.icloud.com", "CALDAV_USERNAME": "me@icloud.com", "CALDAV_PASSWORD": "app-password" }
    }
  }
}
```

## Timezones

- Results are reported in `timezone` (tool argument) or `CALDAV_TIMEZONE`, with an explicit UTC offset, e.g. `2026-11-04T17:30:00+01:00`. All-day events stay plain dates; floating times stay unshifted.
- Recurring events are expanded by this server, not the CalDAV server. The CalDAV server's own expansion breaks with non-IANA zone names: Nextcloud treats `TZID=UTC+02:00` as UTC.
- Fixed-offset zone names such as `UTC+02:00` (written by e.g. the Nextcloud Google bridge) are read as your configured timezone when the offset is one it uses. That way a weekly 17:30 event stays at 17:30 after DST changes. Other offsets are converted exactly.
- Windows zone names (`W. Europe Standard Time`) and vendor-prefixed IANA names (`/mozilla.org/.../Europe/Berlin`) are recognised.

## Notes

- Local times are written as `DTSTART;TZID=<IANA zone>` without an embedded `VTIMEZONE`. Common servers accept this, and Radicale and Nextcloud fill in the definition themselves.
- `update_event` / `delete_event` act on the whole resource. For a recurring event that means the entire series (the master `VEVENT`). The same applies to tasks: completing a recurring task completes the whole series.
- Task reminders are relative to the due time (`TRIGGER;RELATED=END`), or to the start when the task has no due time.
- Whether a calendar holds tasks is shown in `components` from `list_calendars` (`VTODO`). Some servers (e.g. iCloud) keep tasks in separate Reminders lists, and some (e.g. Google) don't expose tasks over CalDAV.
