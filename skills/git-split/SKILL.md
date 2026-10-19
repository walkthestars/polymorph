---
name: git-split
description: Split a dirty git working tree into ordered atomic-commit groups. Use when the working tree holds unrelated changes that must not share one commit; proposes groups as data, never commits.
allowed-tools: Read split_plan
---

# git-split — atomic working-tree splits

Propose-only: this skill predicts commit groups. It never creates
commits — the operator commits each group (one commit per group).

## When to use

- `git status` shows unrelated changes (feature + fix, two files with
  independent edits, mixed refactor + behavior change).
- Before writing a commit message covering changes you cannot describe
  in one sentence — split first.

## How

1. Preview: `read` the path `git://<repo>/split` (default repo is the
   workdir — pass `.` explicitly only when the tool needs it), or call
   `split_plan` with optional `repo` and `deps` (`[{before, after}]`
   file-path ordering edges).
2. One group → one commit, in the returned order. A `Cycle` refusal
   names the files — resolve the dependency by hand, never by
   reordering silently.
3. Unrelated changes in ONE file arrive in one group (same-file
   clustering is the v1 floor) — separate them by hand before
   committing.

## Never

- Never treat groups as commits. Never commit without operator review.
- Never invent dependency edges — pass `deps` only from observed
  imports/calls.
