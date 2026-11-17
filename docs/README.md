# Documentation

Start here to understand Google Management at a high level, then go deeper as needed.

## Reading order

1. **[PRD](prd.md)**: what the product is, who it is for, goals, user stories per phase, requirements,
   success metrics, risks.
2. **[Architecture](architecture.md)**: diagrams: context, processes, modules, flows (sign-in,
   mail → notification, digest), state machines, data model, bus, LLM client, trust boundaries,
   failure modes, evolution by phase.
3. **[Decision records](adr/README.md)**: why the main design choices were made and what was rejected.
4. **Specifications**: the precise contracts:
   - [Calendar](spec/calendar.md): primary-calendar sync, notifications, live agenda and reconnection
   - [Events and topics](spec/events.md): bus semantics, topic catalog, payloads, Gmail ingestion
   - [Data model](spec/data-model.md): tables, statuses, retention
   - [Interfaces](spec/interfaces.md): HTTP routes, Telegram messages, CLI, configuration, rules file, LLM contracts
   - [Security and privacy](spec/security.md): assets, threats and controls, scopes, secrets, data handling
5. **[Implementation plan](plan/README.md)**: workstreams per phase, file ownership, Definition of
   Done, architecture invariants. For people and agents writing the code.
6. **[Running as a service](../deploy/README.md)**: Keychain, launchd, Tailscale, backups.

## Implementation status

| Phase | Status | In one line |
|-------|--------|-------------|
| 0 | ✅ shipped, replaced | Python: Gmail → rules → local LLM summary → Telegram |
| 1 | ✅ done | Rust single binary, rules engine v2 from the mail analysis, Keychain, launchd, backups, metrics, CI |
| 2 | ✅ done | Morning digest at 09:05, model triage + feedback, `gm eval`, health alerts |
| 3 | implemented | Telegram buttons and replies: Translate, Analyze, Reply, with approvals and audit |
| 4 | implemented | Primary-calendar notifications; today's live agenda in the digest |

Backlog: expense ledger, Drive, free-form agent and automations, Bonsai-27B.

## Keeping documents current

- A change to behavior, topics, tables, routes or configuration updates the matching spec in the same
  commit.
- Write current behavior, contracts and architectural rationale that a developer can understand
  from the codebase alone. Keep conversation history, deployment reports, account consent state,
  live verification results and test-run totals in chat or commit messages.
- Defaults picked without the owner are listed in [plan/README.md](plan/README.md#open-questions)
  until confirmed.
- Docs link to the code that implements them; `cargo test` checks docs against the code
  ([`tests/docs.rs`](../crates/gm/tests/docs.rs)).
- Completing a workstream updates the status table in [plan/README.md](plan/README.md).
