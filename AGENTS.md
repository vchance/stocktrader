# Agent instructions — agent-rules-pin

<!-- agent-rules manifest (checked by policy-drift; keep these lines exact) -->
policy: agent-rules/core@0.1.x   ·   profile: PROFILE.md   ·   core-digest: 47e7296ccf51

Read, in this order: (1) `.agent-rules/core.md` — the shared standard this repository follows; (2) `PROFILE.md`; (3) the sections below. Then classify the change (core R2) before editing anything.

## This repo
{{ONE_OR_TWO_LINES: what it is and what an agent cannot learn from the README}}

## Commands
```sh
{{BUILD_CMD}}            # build
{{TEST_CMD}}             # all tests (escalation only; see PROFILE.md › Check selection)
{{TEST_ONE_CMD}}         # one test / one crate / one package
{{LINT_CMD}}             # lint (fails on warnings)
{{FORMAT_CHECK_CMD}}     # format check
git config core.hooksPath .githooks   # once per clone: enables the pre-push guard (core R3)
```

## Layout
{{NON_OBVIOUS_OWNERSHIP_AND_BOUNDARIES}}

## Gotchas
{{GENERATED_FILES, REQUIRED_SERVICES, INTENTIONAL_ODDITIES}}

## Repository additions
<!-- Rules that ADD to the core. Each gets an id so reviewers and the drift check can cite it. -->
- **A1** {{...}}

## Overrides
<!-- Only D-rules may be overridden. Format: id, replacement, why, authority (owner), scope. Delete if none. -->
- overrides: {{Dn}} — {{replacement}} — why: {{...}} — authority: {{owner}} — scope: {{paths or whole repo}}

## Owner decisions
<!-- Ask, don't decide: releases, publishing, dependency majors, schema changes, {{...}} -->
