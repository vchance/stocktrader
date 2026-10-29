# Agent instructions

<!-- agent-rules:begin (managed by agent-rules/scripts/sync.py; edit it there, not here) -->

## Shared rules

These apply to every repo. Repo-specific sections below override them where they conflict.

### Git and worktrees

- Several sessions may run against this repo at once. Never switch the branch of a checkout you
  did not create; work in your own worktree instead.
- Never force-push a shared branch. After a merge, keep the branch; delete it only if asked.
- Subagents started with `isolation: "worktree"` get a worktree under `.claude/worktrees/agent-*`,
  which is left behind if the subagent committed. After merging its branch, run
  `git worktree remove .claude/worktrees/<name>`.
- Before ending a session, run `git worktree list` and remove any `.claude/worktrees/*` entry whose
  branch is merged into `main` and has no uncommitted changes, then `git worktree prune`.
  Never remove a worktree with uncommitted changes or unpushed commits; report it instead.

### Pull requests

Every change reaches `main` through a pull request, and a PR is merged without asking
the owner once all of these hold:

1. **Branch and PR.** Never push to `main`. Branch from an up-to-date
   `origin/main`, one topic per PR. The title says what changes; the body says why, the
   commands you ran to verify it with their results, and what you did not do or could not check.
2. **Independent review.** Spawn the `pr-reviewer` agent with the PR number. It gets the diff and the
   code, not your conclusions. The author never reviews its own PR.
3. **Design review for anything visual.** If the PR changes what a person sees (screens, UI
   components, styles, icons, rendered or charted output, documents meant to be viewed), also spawn
   the `design-ux-reviewer` agent. It runs the built app and looks at it.
4. **Findings resolved.** Fix every blocking and should-fix finding in new commits, then have the
   same reviewer check the fixes. A finding stays unfixed only if that reviewer agrees it can.
5. **Checks green.** Every CI check on the latest commit passes. Where no workflow covers the change,
   the build, lint and test commands this file lists pass locally and are reported in the PR. Never
   skip, disable or edit a check to get green.
6. **Merge.** Once 1 to 5 hold, `gh pr merge <number> --merge` without asking. A PR that misses any
   of them waits for the owner; say in the PR which one and why.

Never force-push a shared branch or rewrite someone else's PR branch. Answer every human review
comment with a fix or a reason.

### Verifying work

- Before calling a task done, run the build, lint and test commands this file lists. If one could
  not run, say so; don't report it as passing.
- Gate on exit codes, not output. `cmd | tail` and `cmd ; echo $?` hide a failing `cmd`.
- Fix warnings and lint findings. Don't disable `-Werror`-style flags, grow a baseline, or add a
  suppression without a one-line reason next to it.

### Boundaries

- Don't edit a sibling repo from here. Flag the change it needs instead.
- Keep secrets out of files, commits and command lines (`ps` shows arguments); pass them through
  the environment.
- Decisions this file reserves for the owner (releases, publishing, major dependency bumps,
  deferred work) are not yours to make. Ask.
- Match the surrounding code's naming, idiom and comment density.

<!-- agent-rules:end -->

## This repo

<!-- One or two lines: what it is, and anything an agent can't learn from the README. -->

## Commands

<!-- Exact commands, in code fences: build, test, a single test, lint, format check. -->

## Layout

<!-- Only the non-obvious parts: which directory owns what, boundaries not to cross. -->

## Gotchas

<!-- Generated files, required services, flaky tests, things that look wrong but are intentional. -->

## Design review

<!-- Read by the design-ux-reviewer agent. Which paths are visual (screens, styles, rendered
     output); the exact commands to build, run and screenshot them; and the brand or design docs to
     check against. Delete this section if nothing in the repo is visual. -->

## Owner decisions

<!-- Things the agent must ask about rather than decide: releases, dependency bumps, deferred work. -->
