# 0007. Google Sheets via drive.file for the ledger instead of Keep

- Status: Deferred (ledger moved to the backlog on 2026-11-16, see [decision log](../decisions.md) P13)
- Date: 2026-11-16

## Context

One of the owner's desired actions is "save the amount to a note". Google Keep would be the natural
place, but the Keep API is only available for Google Workspace (enterprise) accounts, not personal
gmail.com accounts. Access to the owner's Drive should be as narrow as possible.

## Decision

- Record amounts in a **Google Sheet created by the app**, with fixed columns (date, amount, currency,
  counterparty, description, category, mail link, event id, recorded at).
- Use the **`drive.file`** scope, which only grants access to files the app created or the owner
  explicitly opened with it — not the rest of Drive.
- Store the spreadsheet id in `kv_state`; deduplicate by event id in `ledger_entries`.

## Consequences

- Works on personal accounts; the sheet is easy to view, filter and chart.
- The app cannot write to a sheet the owner created manually (unless picked through a Drive picker
  in the future).
- Free-form notes, if needed later, can use a Google Doc created the same way.

## Alternatives considered

- **Keep API:** unavailable for personal accounts.
- **Full `drive` or `spreadsheets` scope to use an existing sheet:** much broader access than needed.
- **Local database only:** no easy viewing/editing on the phone.
