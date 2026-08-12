# Recording & replaying trajectories

> **Cross-platform.** Recording is available on macOS (native
> ScreenCaptureKit), Windows (ffmpeg + `gdigrab`), and Linux (ffmpeg +
> `x11grab`). Replay is cross-platform as long as the recorded artifacts
> are present.

Session-scoped capture of action sequences + pre/post state, suitable
for demos, regression diffs, and training data. Invoked only when the
user explicitly asks to record — the skill does not auto-enable this.

`start_recording` turns on a session-scoped trajectory recorder. While
enabled, every action-tool call (`click`, `right_click`, `scroll`,
`type_text`, `press_key`, `hotkey`, `set_value`) writes a numbered
turn folder under a caller-chosen output directory. Read-only tools
(`get_window_state`, `list_windows`, `list_apps`,
permission probes, agent-cursor getters / setters, and the recording
controls themselves) are not recorded.

**Video is off by default.** `start_recording` captures trajectory evidence
without spawning a video recorder unless you pass `record_video:true`. When
enabled, the main display is written to `<output_dir>/recording.mp4` (H.264 /
30 fps) and finalized on `stop_recording`.

**macOS — native ScreenCaptureKit, zero-config.** On macOS the daemon's
recorder uses `SCStream` + `SCRecordingOutput`, so it inherits the daemon's
Screen Recording grant — no separate
subprocess prompt, no fast-fail, no second TCC dance. Requires macOS
15.0+ (SCRecordingOutput introduced in macOS 15). No ffmpeg needed.

**Windows / Linux — ffmpeg subprocess.** Outside macOS the recorder
shells to ffmpeg with `gdigrab` (Windows) or `x11grab` (Linux). The
binary needs to be on PATH (`winget install Gyan.FFmpeg` /
`apt install ffmpeg`); when missing, the per-turn capture continues
without video and `last_error` carries the install hint. ffmpeg
startup failures fast-fail with a stderr tail in the error.

## Start / stop

Use the raw `start_recording` / `stop_recording` tools alongside the named run
required by `SKILL.md`. These recording controls do not accept a public
`session` field in v0.19.3; the run's actual GUI actions still carry their named
session. The legacy `recording` subcommand group forces video on, so it is
unsuitable when the default no-video behavior is intended.

```
cua-driver-local start_session '{"session":"record-run-1","capture_scope":"auto"}'
cua-driver-local set_agent_cursor_enabled '{"session":"record-run-1","enabled":true}'
cua-driver-local set_agent_cursor_motion '{"session":"record-run-1","idle_hide_ms":0}'
cua-driver-local start_recording '{"output_dir":"~/cua-trajectories/run-1","record_video":false}'
# … run every observation and action with "session":"record-run-1" …
cua-driver-local get_recording_state '{}'
cua-driver-local stop_recording '{}'
cua-driver-local end_session '{"session":"record-run-1"}'
```

Recording requires a running `cua-driver-local serve` because state is
per-process. `output_dir` expands
`~` and is created (with intermediates) if missing. Turn numbering
starts at `1` every time recording is (re-)enabled, regardless of any
existing contents in the directory. State lives in memory only — a
daemon restart resets to disabled.

## What each turn folder contains

Each action writes to `turn-NNNNN/` (five-digit zero-padded counter):

- `before_state.json` and `after_state.json` — application accessibility
  state immediately before and after the action. They carry the same
  `tree_markdown` and `element_count` shape as `get_window_state`.
- `before.png` and `after.png` — target-window images immediately before
  and after the action. Window capture remains scoped to the target when
  another window covers it.
- `evidence.json` — capture status for each phase. Missing expected capture
  has an explicit classification instead of disappearing from the turn.
- `app_state.json` and `screenshot.png` — compatibility aliases for
  `after_state.json` and `after.png`.
- `action.json` — the tool name, full input arguments, result
  summary, result-error flag, pid, click point (when applicable), ISO-8601
  timestamp.
- `click.png` — for click-family actions (`click`, `double_click`,
  `right_click`): a copy of `before.png` with a red marker drawn at
  the click point. **Both addressing modes are covered:** explicit
  `x, y` clicks use the supplied coordinates directly, and
  `element_index`-addressed clicks resolve to the element's center
  via the live AX/UIA cache, then convert to window-local screenshot
  pixels. Absent for non-click tools. It is also absent, and explicitly
  classified as not applicable, when the driver refuses a click before target
  resolution; no input was aimed in that case. A dispatched click whose marker
  cannot be resolved or rendered remains an evidence failure.

## When to use it

- Demos and screen recordings — play the turn folder back to show
  exactly what the agent saw and what it did.
- Replay for regression — re-run the same sequence against a future
  build and diff the new trajectory against the saved one.
- Training data collection — each turn is a
  `(state, action, next_state)` triple ready for offline learning.

## When to invoke it

This skill does **not** auto-enable recording. The client invokes
`start_recording` explicitly when the user asks to capture a session.
If the user says "record this session" or similar, call
`start_recording({output_dir:…, record_video:false})` before the first
action. Pass `record_video:true` only when video is desired, call
`stop_recording({})` when done, and always clean up with
`end_session({session})`.

## Replaying a recorded trajectory

`replay_trajectory({dir})` walks `<dir>/turn-NNNNN/` folders in
lexical order, reads each `action.json`, and re-invokes the recorded
tool with its recorded `arguments`. Optional knobs: `delay_ms`
(pacing between turns, default 500) and `stop_on_error` (halt on
first failure, default true).

```
cua-driver-local start_session '{"session":"demo1","capture_scope":"auto"}'
cua-driver-local set_agent_cursor_enabled '{"session":"demo1","enabled":true}'
cua-driver-local set_agent_cursor_motion '{"session":"demo1","idle_hide_ms":0}'
cua-driver-local start_recording '{"output_dir":"~/cua-trajectories/demo1","record_video":false}'
# … run the workflow …
cua-driver-local stop_recording '{}'
cua-driver-local end_session '{"session":"demo1"}'
# Later: replay against a new build.
cua-driver-local start_session '{"session":"replay-demo1","capture_scope":"auto"}'
cua-driver-local set_agent_cursor_enabled '{"session":"replay-demo1","enabled":true}'
cua-driver-local set_agent_cursor_motion '{"session":"replay-demo1","idle_hide_ms":0}'
cua-driver-local replay_trajectory '{"dir":"~/cua-trajectories/demo1","delay_ms":500}'
cua-driver-local end_session '{"session":"replay-demo1"}'
```

Important caveat: **snapshot-bound element targets do not survive across
sessions.** `element_token` and `element_index` + `snapshot_id` are assigned by
each fresh `get_window_state`, keyed on `(pid, window_id)`. A recorded element
action from yesterday therefore cannot resolve today: the pid, window id, and
snapshot identity are stale. Pixel clicks and keyboard tools without an element
target replay cleanly; element-targeted actions require a live snapshot that
replay does not currently re-emit (read-only tools such as `get_window_state`
are not recorded). For a reliable replay, either
compose the trajectory from pixel + keyboard primitives, or capture
it as a regression artifact (compare the failure/success pattern
across builds) rather than a re-driving script.

If recording is still enabled while replay runs, the replay is
itself recorded into the current output directory — that's the
intended regression-diff workflow.
