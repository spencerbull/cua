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
- [x] Native Wayland overlay now owns one surface per output
  (`9ce93db29`), preserves independent named-session cursors (`61fa09d87`),
  and initializes every configured surface without blocking screencopy
  (`13e48ae27`). Each fix passed a separate independent review.
- [x] Local release install, source provenance, service isolation, exact
  Codex/Claude skill links, three-output layer creation, live Hyprland capture,
  session cleanup, and zero-tick idle behavior verified.
- [ ] Final visible pointer/click/keyboard postcondition after the secure
  Omarchy session is normally unlocked. The lock became secure during the live
  run; credentials were not requested or entered, and the test app/session were
  cleaned up.
- [ ] Final commits pushed.

## Open questions and checkpoints

- Resume the final visible pointer/click/keyboard check after Omarchy IPC reports
  `sessionLocked:false`. Do not attempt to unlock the secure surface.
- Stop if either stream requires changing the stable release channel or the
  v0.19.3 public action schema.

## Evidence

- Combined `git diff --check`, `cargo fmt --all -- --check`, and
  `cargo check -p cua-driver-core -p cua-driver-sdk -p platform-linux
  --all-targets` pass.
- `cursor-overlay`: 44 passed.
- `platform-linux --lib`: 301 passed, 4 environment-dependent tests ignored.
- Native Wayland overlay: 18 focused tests pass, covering three-monitor logical
  routing, cross-output clearing, independent session ownership, ended-session
  tombstones, hotplug/reconfigure initialization, and idle scheduling.
- Skill installer/docs contract: 17 passed.
- Cursor-event contract: 2 passed; session lifecycle: 3 passed.
- Permission policy: 10 passed; daemon-required: 10 passed; prompt
  authorization: 1 passed; session capture scope: 2 passed; private worker: 4
  passed, 1 subprocess-only probe ignored.
- Optimized `cargo build -p cua-driver --release` passes.
- Independent reviewer found no source-level blockers after `13e48ae27`.
- The generic skill validator stops only on this product pack's intentional
  `version:` frontmatter extension; repository contract tests validate the
  versioned pack and live schemas.
- Installed `cua-driver-local get_config` reports version `0.19.3` and exact
  source SHA `13e48ae27df3194042c52a4b3588df727dd73624`; stable `cua-driver`
  remains `0.7.1`.
- The user service is enabled and active with `serve --idle-hide-ms 0`.
  Hyprland exposes correctly sized `cua-agent-cursor` layers on eDP-1, DP-1,
  and DP-2; the stable owner thread used zero CPU ticks over a three-second
  idle window.
- With all overlay layers active, direct `grim` and CUA `get_window_state`
  both completed. This specifically regresses the pre-fix hang caused by
  configured layer surfaces that lacked their first committed buffer.
- The isolated Sway runtime test could not run because `sway` is not installed;
  no package installation was added to this task.
