# 0003. Intent router and fixed recipes before a free-form agent

- Status: Accepted
- Date: 2026-11-16

## Context

Phase 1 lets the owner ask for actions on a mail (translate, analyze, reply, record an amount). The
obvious design is an LLM agent that reads the mail and calls tools. But the mail is written by an
untrusted third party, and an agent that both reads it and holds a "send mail" tool can be steered by
instructions hidden in the mail (prompt injection). The local runtime (mlx-server-runtime) also has
no native tool calling.

## Decision

In Phase 1:

1. An **intent router** classifies only the **owner's words** (buttons skip it entirely) into a fixed
   set of intents.
2. Each intent is a **recipe** written in code. The model inside a recipe produces content only
   (translation, analysis, reply body, extracted amount).
3. **Targets are decided by code** from trusted sources: the reply recipient comes from the original
   mail's `Reply-To`/`From` header; the spreadsheet is the app's own.
4. Anything with an external effect becomes a **proposed action** that the owner approves after
   seeing the full preview; the executor re-validates before acting.

A free-form agent arrives in Phase 3 with a privileged-planner / quarantined-reader split and taint
tracking.

## Consequences

- Prompt injection can at worst produce bad *content* in a preview the owner reviews; it cannot
  choose recipients or trigger actions.
- New capabilities require writing a recipe (more code, less flexibility) — acceptable for a small
  set of high-value actions.
- No dependency on native tool calling.

## Alternatives considered

- **ReAct/tool-calling agent from Phase 1:** most flexible, highest risk; deferred to Phase 3 with
  stronger controls.
- **Recipes without approvals:** faster, but violates the "human stays in control" principle.
