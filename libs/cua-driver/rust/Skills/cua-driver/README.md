# Cua Driver agent skill

This cross-agent skill teaches an AI agent to operate native applications on
macOS, Windows, and Linux with the
[`cua-driver`](https://github.com/trycua/cua/tree/main/libs/cua-driver/rust)
CLI or MCP server.

It covers the canonical snapshot-action-verify loop, exact window addressing,
accessibility and pixel actions, background/foreground delivery, typed browser
automation, session recording, and platform-specific limitations. The skill
defaults to background delivery and requires structured refusal or observed
failure before a caller escalates to foreground input.

## Install this checkout's local driver

This fork skill uses the isolated `cua-driver-local` identity. From the repository
root on macOS or Linux:

```bash
libs/cua-driver/scripts/install-local.sh --release
cua-driver-local doctor
```

Windows PowerShell:

```powershell
./libs/cua-driver/scripts/install-local.ps1 -Release
cua-driver-local doctor
```

The public curl and PowerShell release installers instead create the canonical
`cua-driver` identity. They are valid for upstream releases, but they do not
create `cua-driver-local` and are not the install path for this fork pack.

On macOS, the installed `CuaDriverLocal.app` needs Accessibility and Screen
Recording permission. On Windows, the daemon must run in an interactive user
desktop rather than Session 0. On Linux, the daemon must share the graphical
session and AT-SPI session bus.

## Install the skill

From ClawHub:

```bash
clawhub install @cua/driver
```

Install the staged local pack only for the requested agents:

```bash
cua-driver-local skills install --local --agent codex --agent claude
```

Repeat `--agent` to select targets; unselected agents and unrelated skills are
left alone. `cua-driver-local skills update --local --agent codex --agent claude`
atomically retargets known CUA-owned links after the local installer stages a
new pack, while preserving real directories and unrelated symlinks. Use
`cua-driver-local skills status --local --agent codex --agent claude` to report
source, target, version, or content drift.

## Reading order

- `SKILL.md`: shared contract, tool selection, session identity,
  snapshot-action-verify loop, action ladder, and failure handling.
- `MACOS.md`, `WINDOWS.md`, or `LINUX.md`: host-specific launch, capture,
  accessibility, input delivery, permissions, and refusal boundaries.
- `BROWSER.md`: exact browser-window binding, explicit profile preparation,
  page refs, trust-classified click/type/navigation, and native fallbacks.
- `RECORDING.md`: trajectory evidence, MP4 capture, and replay.
- `EMBEDDING.md`: embedding the driver into another host application.

The agent should load `SKILL.md`, the current platform guide, and only the
cross-cutting guide needed for the task.

## Browser model

Browser work starts from the same native `(pid, window_id)` selection as every
other app. `get_browser_state` binds that exact window to a session-scoped
target and tab, then returns short-lived page refs for `browser_click`,
`browser_type`, and `browser_navigate`.

Setup is never a hidden read side effect. `browser_prepare` requires explicit
approval before launching a driver-managed profile or attaching to an existing
authenticated profile. Trusted pointer input and synthetic DOM clicks are
reported as different routes; the driver refuses instead of silently changing
trust class or foregrounding a standalone browser.

See `BROWSER.md` for the supported surface and exact recovery rules.

## Recording

Session recording captures before/after state, screenshots, action metadata,
and optional MP4 video. macOS uses ScreenCaptureKit. Windows uses ffmpeg with
`gdigrab`. Linux uses compositor-specific capture or ffmpeg with `x11grab`.
Availability is reported honestly when a host dependency or portal grant is
missing. See `RECORDING.md`.

## Updates and source builds

The skill is versioned with Cua Driver releases. After rebuilding from this
checkout, refresh only the selected local links:

```bash
cua-driver-local skills update --local --agent codex --agent claude
```

From a local checkout, `libs/cua-driver/scripts/install-local.sh` installs the
source-built driver as `cua-driver-local` (and `CuaDriverLocal.app` on macOS),
without replacing the upstream release installation. Keep standalone and
embedded identity rules from `MACOS.md` and `EMBEDDING.md`; launching a raw
binary is not a substitute for the stable app identity that owns macOS TCC
grants.

## License

Repository source files are MIT licensed. Copies published through ClawHub are
distributed under MIT-0, as required for ClawHub skills.
