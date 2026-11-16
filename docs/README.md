# Documentation

Start here to understand Google Management at a high level, then go deeper as needed.

## Reading order

1. **[Decision log](decisions.md)**: every product, triage, technical, ops and LLM decision the owner
   made, with the reason. Read this first.
2. **[PRD](prd.md)**: what the product is, who it is for, goals, user stories per phase, requirements,
   success metrics, risks.
3. **[Architecture](architecture.md)**: diagrams: context, processes, modules, flows (sign-in,
   mail → notification, digest), state machines, data model, bus, LLM client, trust boundaries,
   failure modes, evolution by phase.
4. **[Decision records](adr/README.md)**: why the main design choices were made and what was rejected.
5. **Specifications**: the precise contracts:
   - [Events and topics](spec/events.md): bus semantics, topic catalog, payloads, Gmail ingestion
   - [Data model](spec/data-model.md): tables, statuses, retention
   - [Interfaces](spec/interfaces.md): HTTP routes, Telegram messages, CLI, configuration, rules file, LLM contracts
   - [Security and privacy](spec/security.md): assets, threats and controls, scopes, secrets, data handling
6. **[Implementation plan](plan/README.md)**: workstreams per phase, file ownership, Definition of
   Done, architecture invariants. For people and agents writing the code.
7. **[Running as a service](../deploy/README.md)**: Keychain, launchd, Tailscale, backups.

## Where the system is today

| Phase | Status | In one line |
|-------|--------|-------------|
| 0 | ✅ shipped, replaced | Python: Gmail → rules → local LLM summary → Telegram |
| 1 | ✅ implemented (install steps pending) | Rust single binary, rules engine v2 from the mail analysis, Keychain, launchd, backups, metrics, CI |
| 2 | planned | Morning digest at 09:05, model triage + feedback, residency controller, `gm eval`, health alerts |
| 3 | planned | Telegram buttons and replies: Translate, Analyze, Reply, with approvals and audit |
| 4 | planned | Calendar notifications; today's calendar in the digest |

Backlog: expense ledger, Drive, free-form agent and automations, Bonsai-27B.

## Keeping documents current

- A change to behavior, topics, tables, routes or configuration updates the matching spec in the same
  commit.
- A new or changed decision goes into the [decision log](decisions.md) after asking the owner; a
  significant design change also gets a new ADR (supersede, don't rewrite accepted ones).
- Completing a workstream updates the status table in [plan/README.md](plan/README.md).
