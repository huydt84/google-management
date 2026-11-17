# 0003. Intent router and fixed recipes before a free-form agent

- Status: Accepted
- Date: 2026-11-16

## Context

Phase 3 lets the owner ask for actions on a mail (translate, analyze, reply). The
obvious design is an LLM agent that reads the mail and calls tools. But the mail is written by an
untrusted third party, and an agent that both reads it and holds a "send mail" tool can be steered by
instructions hidden in the mail (prompt injection). The local runtime (mlx-server-runtime) also has
no native tool calling.

## Decision

For owner actions ([phase 3](../plan/phase-3.md)):

1. An **intent router** classifies only the **owner's words** (buttons skip it entirely) into a fixed
   set of intents.
2. Each intent is a **recipe** written in code. The model inside a recipe produces content only
   (translation, analysis, reply body).
3. **Targets are decided by code** from trusted sources: the reply recipient comes from the original
   mail's `Reply-To`/`From` header, the subject and threading headers from the same mail.
4. Anything with an external effect becomes a **proposed action** that the owner approves after
   seeing the full preview; the executor re-validates before acting.

A free-form agent is in the backlog; it would need a privileged-planner / quarantined-reader split
and taint tracking.

Code: [`tasks/router.rs`](../../crates/gm/src/tasks/router.rs), [`recipes/`](../../crates/gm/src/recipes/),
[`tasks/approvals.rs`](../../crates/gm/src/tasks/approvals.rs), [`google/compose.rs`](../../crates/gm/src/google/compose.rs).

## Consequences

- Prompt injection can at worst produce bad *content* in a preview the owner reviews; it cannot
  choose recipients or trigger actions.
- New capabilities require writing a recipe (more code, less flexibility) — acceptable for a small
  set of high-value actions.
- No dependency on native tool calling.

## Alternatives considered

- **ReAct/tool-calling agent from the start:** most flexible, highest risk; left to the backlog,
  with stronger controls.
- **Recipes without approvals:** faster, but violates the "human stays in control" principle.
