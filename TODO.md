# CUA Hyprland v0.19 local-control completion

## Goal

Keep the rebased Hyprland fork isolated as `cua-driver-local`, repair its
agent-facing skill pack, and make the agent cursor remain visible for the full
named GUI-control session across pointer and keyboard actions.

## Done criteria

- The source skill pack documents the live v0.19.3 contract and invokes
  `cua-driver-local`.
- `skills install/update --local --agent codex --agent claude` atomically
  installs or retargets only CUA-owned links and reports mismatches accurately.
- The local service uses `--idle-hide-ms 0`; pointer and keyboard actions reveal
  and position the session cursor; `end_session` removes it.
- Static checks, focused tests, broader Rust tests, independent review, local
  reinstall, and live Hyprland verification pass.
- The final commits are pushed to
  `spencerbull/hyprland-cua-driver-v0.19`.

## Streams

- Skill/docs/installer: branch `cua-skill-repair-v019`, worktree
  `/home/sbull/src/github.com/trycua/cua-skill-repair-v019`.
- Cursor runtime/tests: branch `cua-cursor-visible-v019`, worktree
  `/home/sbull/src/github.com/trycua/cua-cursor-visible-v019`.
- Integration: branch `hyprland-cua-driver-v0.19`, worktree
  `/home/sbull/src/github.com/trycua/cua-hyprland-v0.19`.

## Boundaries

- Allowed: local branches/worktrees, source and test edits, local driver/skill
  reinstall, systemd user-service changes, live local Hyprland interaction,
  commits, and push to the existing fork branch.
- Forbidden: upstream pull requests, production deployment, secrets, billing,
  customer data, changing stable `cua-driver`, changing Seform, or linking
  Hermes.
- Preserve unrelated user files, directories, symlinks, and agent skills.

## Required gates

- `git diff --check` and `cargo fmt --all -- --check`.
- Focused skill CLI/parser/link/status tests.
- Focused cursor event/overlay/platform tests.
- Relevant crate checks and tests, then `cargo build -p cua-driver`.
- Independent read-only review of the integrated diff.
- Reinstall `cua-driver-local`, update only Codex and Claude skill links, and
  verify the local service remains isolated and active.
- Live Hyprland pointer, click, keyboard, visibility, and session-cleanup check.

## Current status

- [x] Requirements and ownership boundaries audited.
- [x] Base branch clean at `6555d1eaf09c4812eb8b480396b60ec4f7ca67c8`.
- [x] No existing fork pull request for the branch.
- [x] Skill/docs/installer stream implemented and tested (`f98ac3072`).
- [x] Cursor runtime stream implemented and tested (`b3bcb9d46`).
- [x] Streams integrated and independently reviewed; two documentation-only
  screenshot contract findings were fixed in `04f8fa0b6`, then re-reviewed
  with no remaining blockers.
- [ ] Local install and live Hyprland verification complete.
- [ ] Final commits pushed.

## Open questions and checkpoints

- None requiring user input. Stop if either stream requires changing the stable
  release channel or the v0.19.3 public action schema.

## Evidence

- Combined `git diff --check`, `cargo fmt --all -- --check`, and
  `cargo check -p cua-driver-core -p cua-driver-sdk -p platform-linux
  --all-targets` pass.
- `cursor-overlay`: 44 passed.
- `platform-linux --lib`: 289 passed, 4 environment-dependent tests ignored.
- Skill installer/docs contract: 17 passed.
- Cursor-event contract: 2 passed; session lifecycle: 3 passed.
- Permission policy: 10 passed; daemon-required: 10 passed; prompt
  authorization: 1 passed; session capture scope: 2 passed; private worker: 4
  passed, 1 subprocess-only probe ignored.
- Optimized `cargo build -p cua-driver --release` passes.
- Independent reviewer found no remaining blockers after `04f8fa0b6`.
- The generic skill validator stops only on this product pack's intentional
  `version:` frontmatter extension; repository contract tests validate the
  versioned pack and live schemas.
