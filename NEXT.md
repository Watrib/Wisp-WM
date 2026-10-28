# Next session — build and smoke passed; visual check in progress

## Experimental session lock: protocol tests passed, not security validated

- `[general] session_lock_protocol = false` by default. When enabled at
  startup, advertises ext-session-lock-v1. Custom lock resource dispatch
  guards the exact owning object and confirmed state before forwarding
  `unlock_and_destroy` to Smithay, avoiding its invalid-unlock callback bug.
  While locked, the scene contains only the lock surface over opaque black;
  client keyboard/pointer focus is isolated, shortcuts and IPC dispatch are
  blocked, and lock clients lost after confirmation fail closed. Nested
  confirmation follows submit, direct confirmation follows the matching
  queued black frame's vblank (not a stale prior frame).
- Protocol tests cover default absence, configure/ack and mapped lock
  surface, blocked desktop actions while locked, confirmed unlock, rejection
  of a second lock's invalid unlock, and dead locker fail-closed behavior.
  Fmt check, warning-free clippy, release build and nested smoke passed
  after the follow-up focus/activation changes. Final-source 95-test rerun
  passed. The user subsequently reported successful swaylock nested
  lock/unlock and real-VT locking with both swaylock and Noctalia lock.
  The user observed no desktop flash when locking on the real VT and
  successfully unlocked with the correct password. Wrong passwords were
  rejected (swaylock PAM `invalid credentials`, twice). VT-away/return and
  hotplug were not individually reported; do not claim
  complete security validation.
- Security audit outstanding: owner disconnect during pending configure,
  lock-surface destruction and output hotplug/resize, clipboard/IME and
  client-initiated focus/activation, black frame presentation on direct DRM,
  and further real-locker edge-case tests. Do not advertise
  this as an unattended-session security boundary. Do not auto-switch VTs.
  Desktop focus changes during the lock now restore lock-surface keyboard
  focus; nested host cursor is hidden while locked. XDG activation can no
  longer change the underlying workspace during a lock. These follow-up
  changes passed clippy, release build, nested smoke and final-source tests.
- XWayland (binary `/usr/bin/Xwayland` is installed) is **not integrated**:
  it needs a separate X11 window-manager lifecycle, focus, mapping, clipboard
  and lock-isolation integration. Begin this separate stage after the user's
  successful real-VT locker compatibility test, keeping it opt-in until
  X11 client and lock-isolation tests pass.

## XWayland stage started (experimental opt-in, unvalidated)

- The user authorized the XWayland stage after successful real-VT swaylock and
  Noctalia tests, including wrong-password rejection with swaylock.
- Enabled Smithay's `xwayland` build feature and added
  `[general] xwayland = false` by default. With it set true at startup, Wisp
  now starts XWayland, attaches Smithay's XWM and exports `DISPLAY` only after
  the WM is ready; initial command/autostart are deferred until then so their
  child environment contains the X11 display. First X11 mapping/serial
  association and managed-window
  wiring are implemented but **not yet validated with a real X11 window**.
  The off-by-default path still lacks override-redirect popups, X11 clipboard
  bridging, robust focus/grab and crash recovery; do not call it daily-ready.
- First compatibility refactor in progress: shared close, metadata, focus,
  arrange and interactive-resize paths no longer blindly assume an xdg
  toplevel; X11 windows are not yet registered or exposed. Re-run fmt, clippy,
  tests, release build and nested smoke before considering this batch stable.
- Integration needs the XWayland shell global restricted to its XWayland
  client, `XWayland::spawn` + `X11Wm::start_wm` lifecycle in calloop, serial
  association in either order, X11 map/unmap/configure/override-redirect,
  `Window::new_x11_window` in the existing Space/layout and focus paths,
  X11-to-Wayland clipboard selection and strict lock isolation. Existing
  managed-window code assumes every `Window` has an xdg `toplevel()`;
  remove these assumptions before registering X11 windows. Keep runtime
  opt-in and fail closed if XWayland startup or the XWM fails.
- Validation gates: default has no X11 socket or `DISPLAY`; opt-in nested
  `xclock`/`xterm` (if installed) must show, receive typing and pointer input,
  resize and close normally; second X11 window must focus correctly; native
  Wayland windows and IPC must remain unaffected. Test clipboard in both
  directions separately, then lock with an X11 client visible and confirm no
  X11 content, input, clipboard or keyboard grab can bypass the lock. Test
  XWayland exit/restart and direct VT only after nested behavior passes.
- First nested opt-in `scripts/smoke_xwayland.py` check passed for XWM
  readiness, X11 root query, and real python-xlib window mapping,
  destruction and X11 input focus. An expanded override-redirect popup/IPC
  close check found an association-order race: a buffer committed before
  X11 registration could remain marked unmapped. Register now samples the
  already committed buffer; three repeated nested checks now pass X11 map,
  unmanaged popup and IPC close. One focus assertion was skipped because
  nested Wisp lacked host focus; the other two passed. Physical
  keyboard/pointer input, selection and locking with an
  X11 window visible remain untested. Final-source fmt check, warning-free
  clippy, 95 tests, release build and default/opt-in nested smoke passed.
- Clipboard/primary-selection bridge is implemented in both directions.
  Final-source fmt, warning-free clippy, 95 tests, release build, default
  release nested smoke and opt-in X11 smoke passed. Nested GTK clients passed
  clipboard and primary text transfer X11↔Wayland and live X11-to-Wayland
  ownership replacement for both. A confirmed nested swaylock denied one
  fresh Wayland-to-X11 clipboard request while locked. Retain opt-in XWayland:
  preexisting offers/in-flight transfers, disconnect recovery, unlock-time
  selection restoration, physical X11 input and lock isolation with an X11
  window visible still need testing. The user plans the xterm lock check next.
  The user has since confirmed both clipboard directions in nested foot/xterm
  with `wl-copy`, `xclip`, and `wl-paste`, reported that primary selection
  works, and observed swaylock hide xterm until unlock, after which everything
  returned to normal. This does not establish security against preexisting
  offers/in-flight transfers or direct-VT X11 behavior.
  A nested-only timing harness is now in
  `scripts/smoke_xwayland_selection_lock.py` with a Wayland offer-holder
  example (`examples/selection_probe.rs`); the strict old-offer check gates
  any lock-security claim, the in-flight count stays an informational known
  gap until a cancel path exists, and the opposite direction and direct
  hardware remain outside this harness.
  **Result:** `old-offer` PASSed twice: an X11 clipboard offer granted before
  locking delivered zero bytes when its receive happened after the nested
  locker confirmed the lock. `in-flight` currently FAILS as a diagnostic:
  ~7 MB of a large in-flight X11->Wayland transfer were still delivered after
  lock confirmation. Cause is Smithay 0.7's XWM pumping already-started
  receives with no public cancel API; new receives are still denied while
  locked, and the leak is bounded to in-progress transfers. Until upstream or
  a vendored XWM provides a cancel path, do not present locking as cutting
  all X11 clipboard read paths mid-flight.

## Workspace slide visual revision (opt-in, user check outstanding)

- The user's full-width slide felt cheap. Keep it off by default, but move
  incoming windows only ~24% of the viewport and outgoing ~12%, use a short
  ease-out movement with smooth crossfade, and keep shell/cursor stationary.
  Fmt, warning-free clippy, 95 tests, release build and default nested smoke
  passed. The user subsequently reported the new transition looks good;
  no performance or smoothness measurements were made.

## Previous output-mode and session-lock planning (historical)

- Added optional `[render] direct_mode = "WIDTHxHEIGHT[@HZ]"` on the
  single direct connector, defaulting to driver-preferred. Invalid syntax and
  unavailable connector modes fail rather than silently falling back; this
  setting requires a restart (not a live reload). Fmt, warning-free clippy
  and 92 tests, release build and nested smoke passed. Direct mode hardware
  testing of the requested mode remains pending.
- Full multi-monitor and output scaling have **not** been implemented: input
  coordinates, Space, render scenes, damage trackers, and direct scanout are
  single-output today. Do not call direct_mode multi-monitor support.
- The earlier Smithay 0.7 invalid-unlock issue motivated the owner-checked,
  opt-in protocol described above; this is no longer an unimplemented blocker.
  Wrong-owner and crashed-locker protocol cases are tested, but hotplug,
  pause/resume and other security edge cases still need live validation.

## Held modifier during workspace slides (user hardware retest pending)

- The user reported that while holding Alt, pressing 2 followed by Return did
  not launch the terminal: the slide path synthesized a release of Alt when
  switching workspaces. It now releases held pointer buttons but retains the
  physical keyboard/XKB state. Keyboard focus stays absent throughout the
  transition; releases of keys forwarded before it are accounted for without
  sending them to an unfocused client. Shortcut releases remain intercepted.
- Added regression covering Alt+2 then Alt+Return without releasing Alt, plus
  release of Alt during a slide. Fmt, warning-free clippy, **90 tests**, release
  build and nested smoke passed. The updated
  release binary is ready for user-led direct VT retest, not hardware-tested
  automatically.
- Local glmark2 and niri comparison reports were inspected for reference;
  neither contains Wayland dmabuf protocol tracing, so actual Wisp dmabuf
  imports and performance remain unverified.

## Daily-use batch — software validation passed; hardware/visual still pending

- GLES composition and `OutputDamageTracker` were already present in nested
  and direct modes; no separate acceleration claims. Added opt-in new-window
  fade plus slight entry scale, default off (`window_open_fade_ms = 160`,
  60–400). Fade uses surface alpha and premultiplied decoration colors; opacity
  changes damage the affected elements. Existing opt-in tiled resize spring
  animates tile rearrangements when windows move or open. Hidden scratchpad
  commits no longer trigger redraw; outgoing slide windows do trigger redraw.
  Fmt, clippy, 89 tests, release build and nested smoke passed. Limited nested
  samples are in the local validation records; no GPU timings.

- Opt-in workspace slide (`[layout] workspace_slide`, default false, 80–500 ms):
  temporarily keeps outgoing windows mapped, transforms both workspaces in
  the scene, suppresses client input, cancels cleanly on repeat switch, output
  resize/hotplug or reload, and unmaps outgoing windows at completion. Includes
  geometry and real-client lifecycle tests passed. An opt-in nested smoke
  counted 17 rendered frames for one slide and 0 extra idle frames afterward.
- Single-window scratchpad via IPC actions and optional TOML bindings. Hidden
  scratchpad windows are excluded from hit testing and workspace counts; the
  scratchpad follows the active workspace when shown and hides on switches.
- `--status-stdout` opt-in JSON-lines state snapshots for bars, written on a
  bounded background channel; logging remains on stderr. No stdout protocol
  is enabled by default. Existing `wispctl status/windows/workspaces` remain.
- XDG shell already works; focus-follows-mouse already defaults off. Full
  multi-monitor, output scaling and security-sensitive ext-session-lock are
  **not implemented**. Both backends still use one output and scale 1.
- User authorized fmt, compilation, tests and a safe nested measurement for
  this batch. Single-run five-second steady-state samples (no transitions in
  the measured intervals): default idle 0 redraws; animations-enabled idle 0
  redraws; animated foot 150 frames/0.44 CPU s vs 150 frames/0.43 CPU s.
  These are not paired GPU or animation smoothness measurements and must not
  be presented as an improvement. Direct GPU test still needs a user-approved
  free VT; do not switch VTs autonomously.

## New direct-only DMA-BUF batch (separate from the validated navigation release)

- Smithay linux-dmabuf v4 global is created only when the direct GLES renderer
  exposes importable texture formats. Feedback uses its DRM render node and
  only import formats, not hypothetical scanout formats. v3 clients see legacy
  modifier events; both default and surface feedback are supplied.
- Protocol import callback uses the *same* direct GLES renderer to check actual
  import before accepting a client buffer. Rejected imports notify clients
  through `failed` (nonfatal for asynchronous create); `wl_shm` stays usable.
  When the direct session is paused, imports are rejected. Nested does not
  advertise dmabuf at all; no pointer to its winit renderer was introduced.
- Focused protocol test covers v4/v3 advertisement, feedback delivery, rejected
  import, and shm fallback. `cargo check --all-targets`, the focused test, fmt,
  fmt check, clippy, **81 tests**, release build and nested smoke all passed.
  The new release binary includes this path.
  **Real GPU-buffer creation/rendering has not been hardware-verified**.
- Hardware test requires the user to choose and approve a free VT, and to run
  a genuinely accelerated Wayland client. Do not switch VTs automatically or
  claim a speedup without paired measurements.

## Changes in the validated build

- Removed experimental three-finger workspace swipe and four-finger Exposé
  pinch after the user's unsuccessful direct tests. Removed their config
  fields and the obsolete temporary gesture-test config. No further gesture
  work is planned.
- Added TOML `[extra_bindings]`: retain the default shortcuts while adding or
  overriding individual chords. An explicit `[bindings]` still replaces the
  base map. Both tables are normalized before overrides; duplicate equivalent
  chords within a table remain errors. This uses the existing reload path.
- Added overview-only Tab/Shift+Tab selection and Enter acceptance, using cached
  thumbnail order and the existing focus highlight and exit spring.
- Documented the existing TOML blur and appearance settings.
- Added session-only `toggle-blur` / `toggle-shadows` actions, available to
  bindings and `wispctl`; reload restores configured settings. `status.effects`
  exposes requested effect settings, not proof of shader/hardware success.
- Added `workspace-next` / `workspace-prev` actions and CLI commands, bounded
  at workspace 1 and `general.workspaces`, without wrapping or new default binds.
- Added `[layout] overview_animation = true`; false skips zoom and finishes
  input isolation immediately on exit. Reloading false also settles an active
  overview transition after applying the new layout.
- Added `focus-direction` / `swap-direction` to TOML actions and `wispctl`.
  Focus uses visible geometry (thumbnail geometry in overview), alignment and
  distance, with no wrapping. Tile swaps preserve the focused ID and exclude
  floating/fullscreen/overview cases. Plain arrows navigate overview.
- Added `workspace-back-and-forth`, history shared by actions and activation,
  and last-focused-window memory per workspace. Closed/moved/unmapped and
  fullscreen-hidden remembered windows are filtered when restoring focus.
  Reload prunes out-of-range workspace history; status reports the previous one.
- Added opt-in `[general] autostart` argv lists, launched once after startup,
  never on reload. Uses existing spawn environment/child tracking; failure of
  one command is logged and does not prevent later entries.
- Removed the temporary ID vector from normal focus cycling; overview and
  normal cycling now share an iterator helper. Directional selection also uses
  streaming candidates. No latency, CPU, memory or frame-time improvement has
  been measured. Focus changes during overview exit now keep client input isolated.

The user subsequently authorized fmt, clippy, tests and release build. All passed
with one Cargo job; tests used `CARGO_PROFILE_TEST_DEBUG=0`. The full test suite
has **80 passing tests**. Release compilation took 274 seconds. A first test
run exposed a synthetic-client
ordering issue: queued configure acknowledgements were sent after toplevel
destruction. Draining those events before destroying the role fixed the test;
the targeted regression and subsequent full suite passed.

Added tests cover binding inheritance/overrides,
invalid chords/actions, selection wrapping/missing windows, and keyboard routing
with real Wayland windows.
Further tests authored for workspace boundaries (including u8 maximum and a
single workspace) and instant overview exit/focus restoration with a real client.
Latest authored coverage: directional geometry/ties/extreme coordinates,
cycling, real-client focus/swapping/fullscreen exclusion, per-workspace remembered
focus with closed/moved windows, history pruning on reload, argv validation,
autostart error continuation and child-only environment, and overview arrow keys.
Release smoke subsequently passed: client/protocol warnings, urgency, layouts,
floating/fullscreen, workspace/rules, reload/error handling, idle rendering,
close and clean/bounded shutdown. Visual verification and performance measurements
remain pending. An isolated nested visual session was launched with validated
config `/tmp/opencode/wisp-visual-navigation.toml`; log is
`/tmp/opencode/wisp-visual-navigation.log`. No VT was switched.

## Validation when the user returns

`python3 scripts/smoke.py --release` passed against the newly built binary.
Fmt, fmt check, clippy (`--all-targets -- -D warnings`), tests (`--all-targets`)
and release build are complete; repeat only if further code changes require it.
Then check the example config with the new release binary and try overview
Tab/Shift+Tab/Enter in an isolated nested session. Check TOML extra-binding reload
and invalid-reload retention. Do not switch VTs or touch host clients.
Also exercise effect toggles and their requested status fields, workspace bounds,
instant overview entry/exit, and reloading animation-off during an active
transition. Confirm blur/shadow removal leaves no stale pixels in nested mode.
Exercise directional focus/swaps at edges, in master/dwindle/floating/fullscreen,
and overview arrows; workspace return with closes/moves and config shrink;
startup command order/failure and no relaunch on reload. Measure allocation-free
focus cycling under an equivalent workload before claiming a speedup.

## Research behind the latest batch

- [i3 user guide](https://i3wm.org/docs/userguide.html): directional focus/movement,
  workspace back-and-forth, and startup commands are documented everyday workflows.
- [Sway wiki](https://github.com/swaywm/sway/wiki): configuration and startup
  integration, keyboard-driven window selection, and workspace controls.

These are established-WM workflow references, not evidence of a ranked user-demand
survey. Wisp's additions reuse its existing layout/IPC/spawn infrastructure.

## Subsequent essentials to consider

1. Direct frame-pacing investigation and presentation feedback: measure the
   reported Noctalia skips on hardware before claiming a fix.
2. `linux-dmabuf` import/feedback for accelerated Wayland clients, with a tested
   shared-memory fallback; measure rather than promise lower CPU usage.
3. Session-lock and idle-inhibit support for an everyday laptop session.
4. Configurable output scale/modes, then multi-output support.

Roadmap: Phase 1 decorations → Phase 2 dim/optional blur → Phase 3 smart gaps →
Phase 4 opt-in resize springs (workspace swipe removed) → Phase 5 keyboard/IPC
Exposé and visually confirmed zoom (pinch removed; keyboard selection tests pass,
live visual check pending).
