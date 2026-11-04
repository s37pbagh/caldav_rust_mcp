# caldav_mcp

An [MCP](https://modelcontextprotocol.io) server (stdio) that gives an AI assistant read/write access to any CalDAV calendar: Nextcloud, iCloud, Fastmail, Radicale, Baïkal, SOGo, Google and others.

## Tools

| Tool | What it does |
|---|---|
| `list_calendars` | Lists calendars (URL, name, color, supported components) |
| `list_events` | Lists events in a time range (default: next 7 days). Recurring events are expanded into individual occurrences by default. Optional text filter. |
| `get_event` | Fetches one event by `event_url` or `uid` (+ `calendar`), including the raw ICS |
| `create_event` | Creates an event: summary, start/end, all-day, timezone, location, description, recurrence rule, attendees, organizer, categories, reminders, URL |
| `update_event` | Changes only the fields you pass and keeps everything else. When you move the start, the event keeps its original length. Uses an ETag check so it won't overwrite someone else's changes. |
| `delete_event` | Deletes an event (optional ETag check) |
| `import_ics` | Stores raw iCalendar data (`overwrite` lets it replace an existing event) |

You can give `calendar` as a URL from `list_calendars` or as a display name (e.g. `"Work"`).

Date/time formats: `2026-11-05` (all-day), `2026-11-05T10:00` (local time, uses `timezone`), `2026-11-05T10:00:00+01:00` / `...Z` (absolute).

## Configuration

| Variable | |
|---|---|
| `CALDAV_URL` | **Required.** Server root, principal URL or calendar home. The calendars are discovered from it (also via `/.well-known/caldav`). |
| `CALDAV_USERNAME` / `CALDAV_PASSWORD` | HTTP Basic auth (use an app password for iCloud/Fastmail/Nextcloud with 2FA) |
| `CALDAV_TOKEN` | Bearer token (alternative to Basic auth) |
| `CALDAV_DEFAULT_CALENDAR` | Default calendar name or URL |
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

## Notes

- Local times are written as `DTSTART;TZID=<IANA zone>` without an embedded `VTIMEZONE`. Common servers accept this, and Radicale and Nextcloud fill in the definition themselves.
- `update_event` / `delete_event` act on the whole resource. For a recurring event that means the entire series (the master `VEVENT`).
