---
id: TASK-1
title: Add Nextcloud Deck API support to the MCP server
status: Done
assignee:
  - '@claude'
created_date: '2026-11-12T23:28:00Z'
updated_date: '2026-11-12T23:39:00Z'
labels:
  - nextcloud
  - deck
  - feature
dependencies: []
references:
  - https://deck.readthedocs.io/en/latest/API/
ordinal: 1000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The server currently only speaks CalDAV (events and tasks). Nextcloud users also manage work on Deck kanban boards. Deck's CalDAV integration does not expose stacks, labels or assignees, so the board structure is out of reach today. Adding tools backed by the Nextcloud Deck REST API (/index.php/apps/deck/api/v1.0) lets an assistant read and manage boards, stacks and cards alongside calendar data, reusing the existing Nextcloud credentials.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 MCP tools exist to list boards, list stacks (with their cards) for a board, and get a single card
- [x] #2 MCP tools exist to create, update, move (between stacks/positions) and delete cards
- [x] #3 Cards support title, description, due date, labels and assigned users where the Deck API allows
- [x] #4 Deck tools are only enabled when a Nextcloud base URL and credentials are configured; CalDAV-only setups keep working unchanged
- [x] #5 Deck API errors (auth, not found, permission) are returned as clear tool errors
- [x] #6 README documents the Deck tools and any new configuration variables
- [x] #7 Tests cover request building and response parsing for the Deck client
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Config: new optional NEXTCLOUD_URL (server root, e.g. https://cloud.example.com). Credentials: NEXTCLOUD_USERNAME/NEXTCLOUD_PASSWORD/NEXTCLOUD_TOKEN, falling back to the CALDAV_* credentials. Without NEXTCLOUD_URL no Deck tools are listed or callable; CALDAV_URL stays required.
2. src/deck.rs: DeckClient for /index.php/apps/deck/api/v1.0 (Basic/Bearer auth, OCS-APIRequest: true, JSON). Request building as pure functions (method, path, body) so they can be unit-tested; response-to-JSON conversion as pure functions; HTTP status mapped to clear errors (401 auth, 403 permission, 404 not found / Deck app missing).
3. Tools (src/deck_tools.rs): deck_list_boards, deck_list_stacks (stacks with card summaries, optional archived), deck_get_card, deck_create_card, deck_update_card, deck_move_card, deck_delete_card.
4. Card fields: title, description, due (same date formats/timezone handling as calendar tools, "" clears), labels (id or title, resolved via the board), assigned users (user id). Updates fetch the card and resend the full object, since the v1.0 PUT clears omitted fields; labels/users use the assignLabel/removeLabel/assignUser/unassignUser endpoints; moves use /reorder.
5. stack_id is optional where possible: the card is located by scanning the board stacks.
6. Wire into main.rs (tools/list, dispatch, server instructions), README section, unit tests for request building, parsing, label resolution and error mapping.
7. Live read-only check against a Nextcloud instance if available.

8. (Revised after live test) Moves call /reorder with the TARGET stack in the URL path, because Nextcloud lets the URL's stackId override the body's; deck_move_card verifies the card's stack after the move.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented src/deck.rs (Deck v1.0 client: pure request builders, response conversion, error mapping) and src/deck_tools.rs (7 deck_* tools). Wired into main.rs/tools.rs; tools only listed and callable when NEXTCLOUD_URL is set, credentials fall back to CALDAV_*. README updated. 21 unit tests pass, clippy clean.
Live read-only check against the configured Nextcloud: deck_list_boards and deck_list_stacks (by id and by title) work; 401 (wrong password), 404 (wrong NEXTCLOUD_URL) and missing card produce clear errors. A nonexistent board id returns 403 from Deck itself. Both boards on that instance are empty, so card parsing and all write tools (create/update/move/delete, labels, assignees, archive) are covered only by unit tests against the documented API shapes, not live.

Live write round trip on the Thesis board (with the user's approval): create with labels by title and id, assignee and date-only due; update title/due/labels/assignees; clearing description and due; archive, get without stack hint (found among archived cards), unarchive; bad label title rejected before any write; delete. The first run showed deck_move_card did not change the stack (the URL stackId wins over the body); fixed, then re-tested moves across stacks, to top, to bottom and within a stack. All test cards deleted; board left empty as before.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Added optional Nextcloud Deck support with seven deck_* MCP tools: list boards (with labels and users), list stacks with cards, and get/create/update/move/delete cards. Cards support title, description, due date, labels (by id or title) and assigned users. Boards and stacks can be given by id or title, and stack is optional when locating a card.

Configuration: NEXTCLOUD_URL enables the tools. NEXTCLOUD_USERNAME/PASSWORD/TOKEN are optional and fall back to the CALDAV_* credentials. Without NEXTCLOUD_URL the deck tools are neither listed nor callable, and CalDAV behaviour is unchanged.

Code: src/deck.rs is the v1.0 API client, with pure request builders, response conversion and error mapping (401 credentials hint, 403, 404 with a Deck-installed/URL hint, 400). src/deck_tools.rs holds the tool definitions and handlers. Card updates re-send the full card because the v1.0 PUT resets omitted fields. Labels and users are validated before anything is written.

Verification: 21 unit tests and clippy are clean. A live round trip against a real Nextcloud Deck covered every tool and the error paths, and found and fixed a move bug (the URL stackId overrides the body's).

Risks: tested against one Nextcloud/Deck version; the archived flag depends on Deck's v1.0 update endpoint and the tool reports an error if the server ignores it. Assigning users only works for users who have access to the board.
<!-- SECTION:FINAL_SUMMARY:END -->
