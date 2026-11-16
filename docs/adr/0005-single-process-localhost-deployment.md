# 0005. Single process, localhost only

- Status: Accepted (implementation language amended by [0009](0009-rust-single-binary.md))
- Date: 2026-11-16

## Context

gm serves one person on one Mac. It needs a web UI, OAuth callbacks, background ingestion, bus
workers and (from Phase 1) a Telegram poller.

## Decision

- Run everything in **one Python process**: FastAPI for the web, and asyncio tasks for workers and
  loops, each wrapped in a supervisor that logs crashes and restarts the loop.
- Bind to **127.0.0.1** only. OAuth redirect URIs use `http://localhost`. Remote access, if wanted,
  goes through Tailscale.
- Postgres runs in Docker; the LLM runtimes and (Phase 2) LiteLLM run as separate local processes.
- Phase 2 adds launchd agents for auto-start; the owner runs the `launchctl` commands.

## Consequences

- Simple to start, debug and reason about; one log stream.
- A blocking bug can stall the whole process; Google client calls run in threads (`asyncio.to_thread`)
  to keep the loop free.
- The bus already supports multiple processes (SKIP LOCKED), so workers can be split later without
  redesign.
- Nothing is reachable from the internet, which removes a whole class of attacks.

## Alternatives considered

- **Separate web and worker processes:** more robust isolation, more operational overhead for no
  current benefit.
- **Hosting on a VPS:** would need public endpoints and would move private data off the owner's
  machine.
