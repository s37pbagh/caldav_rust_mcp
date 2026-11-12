---
id: TASK-1
title: Add Nextcloud Deck API support to the MCP server
status: In Progress
assignee: []
created_date: '2026-11-12T23:28:00Z'
updated_date: '2026-11-12T23:30:00Z'
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
