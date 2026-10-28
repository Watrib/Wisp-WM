# Wisp

A small, tiling-first, keyboard-first **Rust / Wayland compositor**, built on **Smithay 0.7**.
Hyprland-inspired workflow, without starting with a large effects stack.

**Early prototype: nested mode is the default; direct DRM mode is experimental.**
There are no claims of being faster than Hyprland: that needs comparable
latency, frame-time, power, and memory measurements on the same hardware.

**Current batch passed fmt, clippy, all 80 tests, and release build:** config
overlays, navigation and effect actions, overview keyboard controls, and startup
commands are included in the new release binary. Release smoke also passed;
live visual checks remain pending. See [`NEXT.md`](NEXT.md) for the remaining
checklist. Detailed validation records and benchmark samples are kept as
local files and are intentionally not tracked in this repository.

## Run inside your desktop

Building requires Linux, Rust 1.85+, `pkg-config`, libwayland, libxkbcommon,
EGL/GLES, libdrm/GBM (Mesa), libseat, libinput, and libudev development libraries.
Nested mode runs inside an existing desktop. `foot` is the default terminal.

```sh
cargo build --release
./target/release/wisp --config config/wisp.toml --command foot
```

This opens **one nested window**, not a new login session. Close that window
or press **Alt+Shift+Escape** to exit. Your desktop configuration is untouched.
Only spawned children receive Wisp's `WAYLAND_DISPLAY` and `WISP_SOCKET`;
the compositor does not mutate the process-wide environment or modify
D-Bus/systemd activation environments.

No config file is required: built-in defaults work. The default config path is
`$XDG_CONFIG_HOME/wisp/config.toml`, or `~/.config/wisp/config.toml`.

```sh
cargo run -- --check-config --config config/wisp.toml
cargo run -- --print-default-config
```

## Direct mode on a free VT

Direct mode drives **one connector on one GPU**, using the driver's preferred
mode (or its first mode). It requires an active local seat through libseat's
logind or seatd backend. Width/height in the TOML file apply only to nested mode;
both backends currently use output scale 1.
To request a specific supported direct-connector mode, set
`[render] direct_mode = "1920x1080@60"` (or `"1920x1080"` to choose the
preferred refresh at that resolution) and restart Wisp. An unavailable mode
fails rather than silently switching to another one. This selects a mode on
the **one** configured connector; it does not add multi-monitor support,
display scaling, or live mode changes. Mode selection has unit tests but
still needs a user-approved direct-hardware test.

Inspect available connectors from your existing desktop first:

```sh
./target/release/wisp --probe-direct
# Explicit device and connector, using the names reported by --probe-direct:
./target/release/wisp --probe-direct --drm-device /dev/dri/<card> --connector <connector>
```

The probe reads cached DRM state through a read-only file, without requesting
DRM master, opening a seat, or starting a renderer. It validates connector/mode/
CRTC selection; it does **not** verify actual scanout or input access.

For the manual hardware test, switch to an unused VT (for example
**Ctrl+Alt+F3**), log in as your normal user, and run:

```sh
cd ~/Projects/wisp  # any checkout location works
./target/release/wisp --backend direct \
  --config config/wisp.toml --drm-device /dev/dri/<card> --connector <connector> \
  --command foot
```

Use the card and connector reported on your system. Omitting those flags picks
the seat's primary/first GPU and its first connected connector. `--seat seat0`
checks the seat supplied by libseat; it does not select a different session.
Direct-only device/connector/seat flags require `--backend direct` or
`--probe-direct`.

Check the following during that VT session:

1. A terminal, borders, and cursor appear at the monitor's native mode. Type,
   scroll, drag/resize, and switch workspaces using the normal Wisp controls.
2. Run `./target/release/wispctl status` inside the terminal. It
   should show `direct-drm-gles`, the selected connector/mode, and an active seat.
3. Use **Ctrl+Alt+F1…F12** to switch away and return to Wisp's VT. Input and
   rendering should resume with a full repaint and no stuck modifiers.
4. On a removable display, check unplug/reconnect. The output global should be
   withdrawn and recreated; other connectors can be chosen when none was pinned.
5. Quit with **Alt+Shift+Escape** or `wispctl quit`, then return to your
   desktop's VT.

The direct backend now advertises **linux-dmabuf-v4** when GLES reports
importable texture formats. Its default/surface feedback uses the GPU render
node and only those format/modifier pairs; no scanout preference is advertised.
Buffer creation is accepted only after that same GLES renderer imports it.
Invalid imports fail through the protocol so clients can fall back to `wl_shm`.
The global is intentionally absent in nested mode, which continues to accept
`wl_shm` buffers. Direct GPU-buffer creation and rendering still need a separate,
user-approved real-hardware test; the existing keyboard/terminal hardware test
does **not** verify dmabuf import or a performance gain.
Fmt, clippy, 81 tests (including v4/v3 feedback and import rejection), release
build and nested smoke passed for this batch; the validation record stays
local.

Direct scanout of client buffers and multi-monitor are not implemented.
Display hotplug and explicit mode selection still need manual hardware
validation; input, VT pause/resume, and composed scanout were tested below.
### Manual hardware validation
Direct mode was tested manually from a free VT on the real GPU
(`--backend direct --drm-device /dev/dri/<card> --connector <output> --command
foot`); the local test log was not preserved and is not tracked. The session
drove the real GPU's device as `direct-drm-gles` at the connector's preferred
native mode; the user verified output, borders, software cursor, typing,
workspaces, and `wispctl status`. Switching VTs showed
`direct session paused; display and input released` and the return resumed
cleanly; quitting left zero remaining clients and stopped cleanly. Three
non-fatal log lines are understood and handled: the unprivileged startup
`Unable to become drm master` warning, `Failed to destroy old mode property
blob` (`os error 2`) after mode commits, and smithay's post-shutdown
`Failed to restore previous state` (`os error 22`).

## ext-workspace-v1 for bars and shells

Wisp implements the staging `ext-workspace-v1` protocol server-side (Smithay
0.7 has no module for it yet), so clients such as the noctalia shell can render
and switch workspaces:

```sh
./target/release/wisp --config config/wisp.toml --command noctalia
```

The advertised shape:

- one `ext_workspace_manager_v1` global; each binding client gets one group and
  one handle per materialized workspace;
- workspaces are **dynamic**: Wisp advertises workspaces 1 through the highest
  active or occupied workspace, plus one empty spare for clicking into the next
  workspace. Vacated trailing workspaces are removed from the bar. Explicit
  `create_workspace` requests may add another empty workspace until the next
  switch. `general.workspaces` is the **maximum index**, not the visible count:
  it defaults to 255 (the `u8` index limit), not nine. The default number-key
  shortcuts still cover 1–9;
- handles report `id` (`wisp-N`), `name` (`N`), `coordinates` (`[N]` as an
  array of uints — the index is widened to 32-bit little-endian on the wire so
  clients reading `array->size / 4` see one uint), capabilities
  (activate | deactivate | assign), urgency, and the active/hidden state bits,
  followed by `done` on every change;
- the group announces the `create_workspace` capability and every output the
  client has bound — both at bind time and late, because Wisp advertises the
  workspace global before its outputs and bars may bind in either order.

Clients switch workspaces with `activate` + manager `commit`; that request is
validated and applied like a native action, so keybind-driven and bar-driven
switches stay in sync. `cargo test --all-targets` covers the full event
sequence, dynamic shrink/retention, and client-created activation with a real Wayland client
(`src/compositor/protocols/tests.rs`).

Manual validation: run noctalia as above and check that its bar widget lists
the workspaces and that clicking a pill switches them. For diagnostics, inspect
`~/.cache/noctalia/noctalia.log` or run with `NOCTALIA_LOG_LEVEL=debug`.
If wire-level tracing is needed, redirect `WAYLAND_DEBUG=1` output to a file;
piping its unbounded output to a terminal can stall the session.

## Layer-shell (zwlr-layer-shell-v1)

Bars, wallpapers and dashboards are `zwlr_layer_shell_v1` surfaces, and they are
first-class in the scene:

- Every layer surface is attached to the single output's layer map at
  registration time; surfaces without a buffer render as nothing. The map
  tracks their anchored geometry and exclusive zones.
- The surface's first commit is answered with the **initial configure**
  carrying the anchored size (width/height from `set_size` where the client
  anchors neither edge pair, output size where it does). This is spec-mandated
  and wlr-layer-shell clients stall without it.
- Stacking is `background` / `bottom` below windows, then `top` / `overlay`
  above them; layer surfaces receive frame callbacks each rendered frame.
- Pointer hit-testing resolves layer surfaces in that same order, so clicking
  the bar or wallpaper reaches the panel instead of focusing a window beneath
  it; dragging bar regions does not start window moves.

`cargo test --all-targets` drives real clients through the anchored-bar shape
(`configure` = 40x768 at x=1240 on a 1280x768 output), the full-screen
wallpaper shape, and the unmap/remap cycle.

## Build cache

Cargo automatically uses the project-local `scripts/sccache.sh` wrapper. It
prefers `.tools/sccache`, then `sccache` on `PATH`, and runs the compiler directly
if neither exists. The optional installer downloads **sccache 0.19.0** from the
official Mozilla release and checks its pinned SHA-256 before installation:

```sh
python3 scripts/setup-sccache.py  # Linux x86-64; other platforms can install sccache themselves
cargo build --release
scripts/sccache.sh --show-stats
# Bypass the cache for one build:
WISP_NO_SCCACHE=1 cargo build
```

The default cache is `.cache/sccache`, capped at **1 GiB**, with a project-local
server socket and config. `.tools/` and `.cache/` are ignored; no shell profiles
or system packages are changed. Explicit `SCCACHE_*` environment settings can
override these defaults.

Existing Cargo artifacts are kept: an unchanged build that Cargo skips needs no
cache lookup. Debug builds retain Rust's incremental compilation, which sccache
cannot cache; dependencies and non-incremental library builds remain eligible.
Release libraries are non-incremental by default. Executable linking is not
cacheable. To opt out of incremental compilation for a debug library build, use
`CARGO_INCREMENTAL=0 cargo build`. See the [upstream Rust caching caveats](https://github.com/mozilla/sccache/blob/v0.19.0/docs/Rust.md).

## Implemented

- Nested winit backend with GLES composition and EGL buffer-age damage tracking.
- Explicit direct DRM/GBM/GLES backend, libseat pause/resume, libinput keyboard/
  pointer input and LEDs, single-output udev hotplug, and page-flip pacing/recovery.
- Dynamic, aspect-aware **dwindle**, **master/stack**, and **floating** layouts.
- Per-window floating/fullscreen, focus cycling, tile swapping, numbered workspaces.
- Keyboard bindings, click-to-focus, optional focus-follows-mouse, pointer scrolling.
- Modifier+left-drag moves; modifier+right-drag resizes, detaching tiled windows.
  Client-requested move/resize validates the input serial and originating client.
- Client cursor surfaces, popup positioning/grabs, clipboard and primary selection
  **within Wisp** (not bridged to the host).
- wlr layer-shell surfaces: anchored sizing with exclusive zones, spec-mandated
  initial configures, background-to-overlay stacking in the scene, frame
  callbacks, and pointer/click resolution into panel surfaces.
- Compositor cursor shapes, fractional-scale negotiation with viewporter, and
  committed toplevel icon metadata (available through IPC, not drawn in a taskbar).
- XDG activation with one-shot, expiring tokens and input/focus validation;
  background attention requests mark a window urgent instead of stealing focus.
- Text-input-v3 / input-method-v2 integration, including IME preedit/committed
  text, surrounding text, keyboard grabs, and candidate popup lifecycle.
- TOML config, configurable colors/gaps/borders, XKB settings, argv launch commands,
  exact-app-id / title-substring window rules, explicit live reload.
- GPU decoration effects rendered in the GLES scene: **anti-aliased rounded
  corners**, **rounded gradient borders**, and **gaussian soft shadows** for xdg
  toplevels, each configurable and each degrading to the previous rendering path
  when shader compilation fails (see the next section).
- Newline-delimited JSON IPC and `wispctl`; separate sockets per instance.
- Bounded graceful shutdown, explicit exit reasons, and nonzero runtime-failure exits.

Phase 3 smart gaps can be enabled with `[layout] smart_gaps = true`. With
exactly one tiled window on the current workspace, Wisp omits the outer gap;
zero or multiple tiled windows keep `outer_gap`. Floating windows and
fullscreen windows keep their existing placement policy. It is off by default
pending a live visual check. Phase 4 visual-only resize springs are available
with `[layout] spring_resize = true` and `spring_frequency_hz = 4.0` (valid
1–12). They are off by default pending live visual and cost checks. Window
configures and pointer hitboxes use the target immediately; the shown window,
border, shadow and dim interpolate to it. Backdrop blur is suppressed while
a window is moving because its framebuffer capture coordinates do not follow
the visual transform. In one nested llvmpipe two-foot test, opt-in springs
generated 25 additional frames after the second window mapped, costing 23 ms
total render-side wall time (~0.92 ms/frame; composition ~0.78 ms/frame) and
36.7 ms total host-present wall time, with 0 render-side calls over 16 ms.
The springs-off control generated no post-map transition frames. Both runs
returned to zero idle redraws; visual quality and direct-GPU cost are not yet
confirmed.

Phase 4's experimental three-finger workspace swipe was removed after a direct
touchpad test did not produce a workspace switch. Workspace switching remains
available through keyboard bindings, IPC, and the workspace bar.

Phase 5 Exposé (current-workspace overview) is toggled with **Alt+e** or
`wispctl toggle-overview`; Escape exits. **Tab / Shift+Tab** cycle the highlighted
thumbnail in grid order, wrapping at either end; **arrow keys** select a neighbor
in the thumbnail grid without wrapping; **Enter** (including keypad
Enter) exits into the highlighted window. These unmodified overview keys take
precedence over configured bindings while overview is open. Windows are drawn as aspect-preserving
thumbnails above a dimmed desktop shell (including layer-shell top/overlay
surfaces). Entering and leaving overview uses the same critically damped
geometry spring as opt-in resize animations (`spring_frequency_hz`, default
4.0), even when `spring_resize` is off. Set `[layout] overview_animation = false`
for instant entry/exit; the default is `true`. Client input and normal layer stacking
are restored only after the exit motion settles. Hovering a thumbnail updates
logical focus and its border without
closing overview; a left-click on a thumbnail exits into the selected window.
An empty-space click leaves overview open.
In one nested llvmpipe two-foot test, the overview zoom used 21 rendered frames
on entry (~20 ms cumulative render-side wall time, 17 ms composition) and 24
on exit (~25 ms render-side, 19.5 ms composition); no render-side frame took
over 8 ms, and the post-exit idle interval rendered zero frames. Seven enter
calls exceeded 16 ms end-to-end due to nested host presentation waiting, not
slow CPU-side rendering. This is a single directional sample, **not** proof of
visual smoothness or direct-mode timing.
Client keyboard/pointer input is suspended while overview is open. It renders
only on changes, not continuously, and does not show windows hidden by a
fullscreen window or on other workspaces. Experimental four-finger pinch entry
was removed after a direct touchpad test did not open overview; the device
reported no four-finger capability. There are no compositor touchpad gesture
shortcuts. Overview labels are still pending. Thumbnail positions are cached
until layout changes,
so hovering them does not recalculate the grid or allocate on each mouse move.
An exploratory 12-second two-foot + Noctalia stress run with overview active
showed **0 render-side calls over 16 ms** and overview idle remains damage-free;
the before/after run had differing client commits and concurrent host-terminal
activity, so it is **not** a controlled speed comparison. The user liked the
initial thumbnails; the shell layering and hover-focus refinements await
their visual confirmation.

## Window decoration effects

Toplevel windows can be drawn with rounded corners, a border ring, and a soft
shadow. Layers (bar, wallpaper), the cursor, and popups are never decorated;
fullscreen windows skip decorations entirely.

```toml
[decoration]
border_width = 2
corner_radius = 8 # logical px; 0 disables rounded corners
active_border = "#89b4fa"
inactive_border = "#313244"
dim_strength = 0.3 # 0..1; black overlay on inactive toplevels' content, 0 disables

[decoration.gradient] # optional dual-color border
enabled = false
from = "#89b4fa" # where the angle starts
to = "#f5c2e7" # where the angle ends
angle = 0 # degrees; 0 = top to bottom, 90 = left to right

[decoration.shadow]
enabled = true
sigma = 8 # gaussian blur width (logical px); drawn area is window + 2.5 sigma
strength_active = 0.55 # 0..1
strength_inactive = 0.3
color = "#000000"

[decoration.blur] # optional window-backdrop blur; OFF by default
enabled = false
passes = 2 # dual-filter Kawase downsample/upsample passes, 1..4
offset = 2.0 # sampling radius, 0..8
```

How it renders (Smithay 0.7 public APIs):

- Rounded corners clip window surfaces through
  `GlesFrame::override_default_tex_program` with an SDF alpha mask; otherwise
  window elements draw unchanged.
- The border ring and shadow are custom pixel-shader elements
  (`compile_custom_pixel_shader` + `PixelShaderElement`). They are stored per
  window and only resized/re-uploaded when geometry, focus, or colors change,
  so idle frames stay damage-free.
- Inactive windows are dimmed with a translucent black overlay exactly over
  their content rect: the overlay is pushed in front of the window's own
  elements but behind everything above, so only that window darkens. It is
  zero-allocation (a solid render element built per damaged frame), and
  fullscreen windows are never dimmed.
- Optional Kawase backdrop blur samples the already-composited background
  behind each normal window when it draws, downsamples and upsamples it with
  GPU textures, and lets Smithay draw the result with normal damage clipping.
  Fullscreen windows, popups, and Noctalia layer surfaces are not blurred;
  this is **not** a Noctalia launcher/clipboard blur switch. It is off by
  default because it copies and filters an area on each damaged frame, which
  can be costly on llvmpipe and older GPUs. Measured there (12 s, one
  translucent 30 fps foot plus noctalia, identical configs except the flag):
  frame count similar (415 vs 374), CPU +25% (1.34 vs 1.07 s), compose wall
  +60% (377 vs 235 ms), present wall ~7.6× (1.31 s vs 173 ms), with zero shader
  fallback warnings. It stays disabled by default; enable it per test to judge
  the cost on your own hardware.
- With `corner_radius = 0` and the gradient disabled, the border uses the
  previous solid-color buffer path unchanged; if only the shadow is requested,
  it draws alongside it.
- If any shader fails to compile the warning is logged and that effect is
  disabled (borders fall back to the solid buffers, corners fall back to
  square).

Known limitations:

- Windows made only of single-pixel buffers are drawn via
  `GlesFrame::draw_solid`, which the texture-program override cannot reach;
  their corners remain square (terminals using shm buffers are clipped).
- The rounding mask does not compensate for y-inverted buffer textures
  (`buffer()` is not public on `WaylandSurfaceRenderElement` in Smithay 0.7),
  so such textures show a mirrored mask on the rare subsurface that uses one.
- Content rect anchoring for the new decoration paths uses
  `element_location + window.geometry().loc`; the legacy solid border keeps
  anchoring on `element_location` only, which visually drifts for the rare
  windows with a non-zero `geometry().loc` bounding offset.
- Outputs run at compositor scale 1 today; effect areas share that assumption
  like the rest of the decoration code.

## Default shortcuts

`Mod` is **Alt**, to avoid common host-desktop Super bindings. Change
`general.modifier` to `"super"` if desired. A host desktop can still consume
shortcuts before Wisp sees them.

| Shortcut | Action |
|---|---|
| Mod+Return | Terminal |
| Mod+j / Mod+k / Mod+Tab | Next / previous / next window |
| Mod+Shift+j | Swap focused tile with the next tile |
| Mod+q | Close focused client |
| Mod+Space | Toggle focused window floating |
| Mod+f | Toggle fullscreen |
| Mod+e | Toggle Exposé overview |
| Tab / Shift+Tab | Next / previous thumbnail (overview only) |
| Arrow keys | Directional thumbnail selection (overview only) |
| Enter / Escape | Accept selection / exit overview (overview only) |
| Mod+d / Mod+m | Dwindle / master layout |
| Mod+1…9 | Switch workspace |
| Mod+Shift+1…9 | Send focused window to workspace |
| Mod+left / right drag | Move / resize (makes the window floating) |
| Mod+Shift+r | Reload config |
| Mod+Shift+Escape | Exit Wisp |
| Ctrl+Alt+F1…F12 | Switch VT (direct backend, standard XKB mapping) |

See [`config/wisp.toml`](config/wisp.toml) for customization. An explicit
`[bindings]` table **replaces**, rather than merges with, default bindings.
`[extra_bindings]` adds to that base map (or the defaults when `[bindings]` is
omitted). A matching shortcut overrides the base action, including equivalent
spellings such as `Alt+p` and `Mod+p` when `general.modifier = "alt"`.
Duplicate equivalent chords within either table are rejected.
Invalid reloads keep the old configuration. Rules apply on the first mapped
buffer; reloading does not retroactively reapply rules to existing windows.
Existing workspace layout choices survive reloads; initial nested window size
is startup-only. FPS, decoration, gap, keyboard, and launch settings update live.
Configured startup commands are only launched on the next compositor start.

### TOML customization

Edit `~/.config/wisp/config.toml`, or the file passed with `--config`. For example:

```toml
[decoration.blur]
enabled = true # optional; blur remains off in the defaults
passes = 2     # 1..4
offset = 2.0   # 0..8

[extra_bindings]
"Mod+p" = { name = "spawn", argv = ["foot"] }
"Mod+e" = { name = "toggle-overview" }
"Mod+Shift+r" = { name = "reload" }
"Mod+b" = { name = "toggle-blur" }
"Mod+Shift+s" = { name = "toggle-shadows" }
"Mod+Right" = { name = "workspace-next" }
"Mod+Left" = { name = "workspace-prev" }

[layout]
overview_animation = true # false: instant overview, no zoom
```

Merge these entries into existing tables rather than repeating a table header.
Reload with **Mod+Shift+r** or `wispctl reload`. Other existing options include
`decoration.dim_strength`, `decoration.corner_radius`, `[decoration.shadow]`,
`[decoration.gradient]`, and `[layout]` gaps, smart gaps, and resize springs.
The example config lists their defaults. Blur applies to normal windows;
it is suppressed during overview and animated window movement.

`toggle-blur` and `toggle-shadows` change the running session only; they do not
rewrite the TOML file. Reloading restores the configured values. Their requested
enabled state is exposed in `wispctl status` under `effects` (rendering may still
suppress an effect or fall back if a shader is unavailable).
`workspace-next` / `workspace-prev` move one numbered workspace and stop at
1 / `general.workspaces`, without wrapping. These actions have no new default
shortcuts; the examples above add them using `[extra_bindings]`.

### Keyboard navigation and workspace return

`focus-direction` selects a visible window in the requested direction, preferring
neighbors aligned on the perpendicular axis, then the closest edge. It does not
wrap. In overview it uses thumbnail positions. `swap-direction` exchanges two
tiled slots and keeps the same window focused; it does nothing for floating or
fullscreen windows, floating layout, or while overview is active/exiting.

Add these to your `[extra_bindings]` table (directions: `left`, `right`, `up`, `down`):

```toml
[extra_bindings]
"Mod+Ctrl+Left" = { name = "focus-direction", direction = "left" }
"Mod+Ctrl+Right" = { name = "focus-direction", direction = "right" }
"Mod+Ctrl+Up" = { name = "focus-direction", direction = "up" }
"Mod+Ctrl+Down" = { name = "focus-direction", direction = "down" }
"Mod+Ctrl+Shift+Left" = { name = "swap-direction", direction = "left" }
"Mod+Ctrl+Shift+Right" = { name = "swap-direction", direction = "right" }
"Mod+Ctrl+Shift+Up" = { name = "swap-direction", direction = "up" }
"Mod+Ctrl+Shift+Down" = { name = "swap-direction", direction = "down" }
"Mod+grave" = { name = "workspace-back-and-forth" }
```

`workspace-back-and-forth` returns to the last different workspace; repeating
it alternates between the two. Selecting the current workspace does not replace
that history. Keyboard, IPC, workspace-bar and authorized activation switches
share the history. `wispctl status` exposes `previous_workspace` (null initially).
Each workspace remembers its last focused window, with a fallback when that
window closes, unmaps, moves, or is hidden by fullscreen. History is session-local;
reload discards references to workspace numbers outside the new configured limit.

### Startup applications

```toml
[general]
autostart = [
    ["foot", "--title", "Wisp startup"],
]
```

The default is empty. Commands are launched in list order once Wisp is ready,
in both nested and direct mode, using the same child environment as `spawn`.
Wisp does not wait for one application to finish before launching the next.
A failed launch is logged and subsequent entries continue. Each entry is an
argument list, with no shell expansion of `$VARIABLE`, `~`, pipes, or quoting.
`--command` is an additional launch, so listing the same application in both
places starts it twice. Reload validates/stores startup configuration but never
relaunches it. Wisp does not automatically restart exited startup processes.

## IPC

Inside a Wisp terminal, `WISP_SOCKET` is already set. From your host desktop,
pass the socket path printed in Wisp's startup log.

```sh
./target/release/wispctl --socket /run/user/1000/wisp-wayland-2.sock status
# Remaining examples assume WISP_SOCKET is set:
./target/release/wispctl windows
./target/release/wispctl workspaces
./target/release/wispctl spawn foot
./target/release/wispctl layout master
./target/release/wispctl master-ratio 0.6
./target/release/wispctl toggle-floating
./target/release/wispctl toggle-blur
./target/release/wispctl toggle-shadows
./target/release/wispctl workspace-next
./target/release/wispctl workspace-prev
./target/release/wispctl workspace-back-and-forth
./target/release/wispctl focus-direction right
./target/release/wispctl swap-direction left
./target/release/wispctl workspace 2
./target/release/wispctl move-to-workspace 1
./target/release/wispctl reload
./target/release/wispctl quit
```

One request and response per Unix connection, each ending with a newline:

```json
{"command":"dispatch","action":{"name":"layout","kind":"master"}}
```

```json
{"ok":true,"data":{}}
```

Queries: `status`, `windows`, `workspaces`. Dispatchable actions are defined
in [`src/protocol.rs`](src/protocol.rs). Failed requests return `ok: false`
and an `error` string; `wispctl` exits nonzero. The socket is mode `0600` under
`XDG_RUNTIME_DIR`, removed on normal shutdown. The IPC endpoint deliberately
allows starting arbitrary programs: **do not expose it over the network**.
Requests are limited to 16 KiB; slow/incomplete IPC input never runs on the
compositor thread. A single IPC worker handles one request at a time, so a slow
peer can delay other IPC peers until its timeout, but not rendering/input.

## Shutdown and diagnostics

Closing the host window, using the quit binding, or running `wispctl quit` asks
every toplevel to close, including windows on hidden workspaces. Wisp continues
dispatching and flushing Wayland events for up to **two seconds**, allowing
clients' final round trips and reaping exited child processes before dropping
the display. New connections and non-quit actions are rejected during cleanup.
The IPC quit reply is written before shutdown starts, even with no clients.
The quit command acknowledges the request; it does not wait for cleanup to end.

Logs show `Wisp shutting down` and `Wisp stopped` with a reason and remaining
client/child counts. An expired grace period is logged; unresponsive clients
are then disconnected rather than hanging exit indefinitely. Close requests
can cause applications to ask about unsaved work, but this prototype does not
wait beyond the deadline or forcibly terminate arbitrary spawned processes.
Renderer, Wayland dispatch, and event-loop failures are logged and return a
nonzero process exit status, not a successful-looking quit. Abrupt signals,
crashes, or an unusable event loop cannot guarantee graceful client cleanup.

The previously reported `foot` warnings about XDG activation, fractional scale,
server-side cursors, toplevel icons, and text input are addressed by implementing
those protocols, not by filtering stderr. The single output still uses
scale **1** (fractional-scale value **120**); this does not add configurable
HiDPI scaling or host-scale following. Nested cursor shapes use the host's theme;
direct mode loads Xcursor images. Clients may supply their own cursor surfaces. Icon names and
buffer counts appear in `wispctl windows`; Wisp has no icon-bearing taskbar yet.

IME composition requires a compatible **input-method-v2 provider running inside
Wisp**. Without a provider, ordinary keyboard input continues normally. No IME
is installed or autostarted, and Wisp does not bridge or reconfigure the host's
IME, D-Bus, or activation environment. Protocol round-trip tests use a separate
synthetic provider; a full external IME engine is not yet integration-tested.
**Only run trusted clients:** this personal prototype exposes input-method-v2
to connected clients, and an IME can access surrounding text and grab keyboard
input. Per-client/sandbox privilege policy remains future work.

Attention requests without a valid input/focus serial can mark only the
requesting client's own window urgent. Such windows use the active-border color
and expose `urgent` in window/workspace IPC queries; focusing them clears it.
Tokens expire after ten seconds, are single-use, and are bounded in number.

`Broken pipe` means the Wayland connection was lost; check Wisp's exit reason to
distinguish a requested shutdown from a runtime failure. Client diagnostics are
not suppressed. `foot` may still report `slave exited with signal 1 (Hangup)`
when its window is closed: foot sends SIGHUP to its terminal process group. That
is separate from a lost Wayland connection; exiting the shell normally avoids
that particular signal warning.

```sh
RUST_LOG=wisp=debug,smithay=warn cargo run -- --config config/wisp.toml --command foot
```

## Performance approach

- **On-demand, capped frame scheduling**: changes queue a one-shot timer;
  idle desktops do not run a repeating render loop. Direct mode allows only one
  in-flight page flip and retains redraw requests while waiting for completion.
  Its one-second watchdog checks for stalled flips without rendering while idle.
- Commits on hidden or unmapped xdg toplevels do not request a frame. Neither
  do synchronized subsurface commits until their parent commits; layer,
  cursor, and popup commits retain conservative redraw behavior.
- **Damage tracking and buffer age**: unchanged content is not intentionally
  redrawn, and swaps are skipped when Smithay reports no damage.
- **Persistent surface texture and border identities**: reuse Smithay's caches
  rather than uploading or inventing new render objects every frame. Both
  backends also retain the scene-element vector's capacity between frames
  (dropping stale element references when rebuilding the scene).
- Config is read at startup or explicit reload, never per frame or on ordinary
  input events. Reload currently parses the small TOML file on the main thread.
- Keyboard focus cycling now scans eligible windows without building a temporary
  ID vector. Directional selection also scans existing geometry without allocating
  candidate lists. This removes those allocations by construction; its effect on
  latency/CPU use has not been measured.
- Resize and overview share a bounded spring animation that stops when settled.
  XWayland is still experimental and off by default; optional Kawase blur is disabled by default
  (measured cost above).

`wispctl status` includes the backend name and direct device/seat/output details,
connected-client count, shutdown state,
rendered/no-damage frame counts, client commit/host redraw counts, and the last
redraw-cycle wall time in microseconds. Cadence diagnostics: cumulative
CPU-side render work (`render_cpu_us`, with per-call counts over 8/16 ms,
`cpu_over_8ms`/`cpu_over_16ms`), the same end-to-end cycle including nested
present parking (`render_over_8ms`/`render_over_16ms`), and direct-only page
flips timed at vblank (`flip_wait_us`, `flip_over_25ms`, `flip_over_40ms`).
It also reports cumulative wall time
spent building the scene, binding buffers, composing/damage tracking, and
submitting/presenting frames, plus the count of invisible commits that did not
schedule a render. Subtract two status samples to profile a workload.
These are diagnostics, **not input-to-photon latency or GPU execution time**.
Nested launcher pacing on a 60 Hz host was compared with eight successful
Noctalia launcher toggles per run (same shell/workload, llvmpipe): setting
`render.max_fps = 144` yielded 25/139 redraws above 16 ms and ~1.05 s total
host-present wall time; `max_fps = 60` yielded 0/113 above 16 ms and ~97 ms
present wall time. Both runs had 0 CPU-only redraws above 16 ms. The example
and built-in default now use 60 fps; tune this cap to your display refresh.
Reloading `render.max_fps` also updates the refresh advertised to nested
clients; direct mode continues to advertise the connector's real refresh.
This measured nested present parking, **not** direct-mode flip timing or
visual confirmation that Noctalia animation skips are gone. Direct flip
counters still need a user-approved VT test.
Window creation, layout changes, and rendering
still allocate; this is not an allocation-free compositor.

For a repeatable CPU/idle sample while Wisp and its shell are running:

```sh
python3 scripts/bench-nested.py --socket /run/user/1000/wisp-wayland-2.sock --settle 20 --seconds 10
python3 scripts/bench-nested.py --socket /run/user/1000/wisp-wayland-2.sock --animate-foot --fps 30 --settle 20 --seconds 12
python3 scripts/bench-nested.py --socket /run/user/1000/wisp-wayland-2.sock --animate-foot --hidden-foot --fps 30 --settle 20 --seconds 12
```

Baseline on this machine (niri host, nested Noctalia, 1280×768, Mesa llvmpipe,
2026-10-25): after a 20-second settle, **9 rendered frames**, **9 client commits**,
**0 no-damage renders**, **0.01 process CPU-seconds** and ~160,556 KiB RSS over
10 seconds. This is an *idle* sample, not a throughput or GPU benchmark;
compare with the same output size, compositor, and client workload.
After the scene-vector reuse change, the same setup sampled **15 rendered frames**,
**15 commits**, **0 no-damage renders**, **0.03 process CPU-seconds**, and
~160,784 KiB RSS over 10 seconds. Both runs were near idle with different
client activity: these short samples do **not** establish a CPU or memory
speedup, only that the change preserves low idle work while eliminating
per-frame scene-vector allocations. Profile a sustained workload before making
performance claims.
On the current default build (2026-10-26), another 20-second-settled,
10-second Noctalia-only idle run again counted **9 rendered frames / 9 commits**,
**0 no-damage redraws**, **0.01 process CPU-seconds**, and **159,044 KiB RSS**
**0.01 process CPU-seconds**, and **159,044 KiB RSS**. Different sessions and
allocator state make
small RSS differences meaningless; the important observation is no idle
render loop or additional default-on effect work.

An initial mixed-load pass (Noctalia, an animated foot at 30 Hz, and manually
opened/resized foot windows) produced **418 rendered frames**, **741 commits**,
**1.25 Wisp CPU-seconds** in 12 seconds at 1280×768. Since the manual window
activity varied during this run, it is a stress check, not a controlled
before/after comparison. Use `--animate-foot` without other interaction for a
comparable controlled workload.

With only Noctalia and the scripted foot active, the batch before the
visibility/sync-deferral change rendered 431 frames with 651 commits and used
1.28 Wisp CPU-seconds. The change itself sampled the same workload at
**372 rendered frames** (~31 Hz, tracking foot's cadence instead of forcing
Noctalia churn) with **732 commits, 360 of which no longer schedule frames**
(sync subsurfaces wait for their parent; invisible commits are dropped),
process CPU **1.16 s**, and present-wall **0.10 s** versus the earlier 2.68 s.
Phase totals over the 12-second window: 0.478 s scene, 0.014 s bind, 0.197 s
compose. Presentation includes waiting for the nested host/EGL swap, not just
Wisp CPU work. These are single-execution, same-workload comparisons on
llvmpipe, not averaged trials; treat them as directional. A hidden-foot
baseline rendered only 10 frames in 12 seconds (~0.01 CPU-seconds) because
foot already self-throttles without frame callbacks; suppressing hidden commits
mainly helps clients that do *not* self-throttle, and no foot speedup is
claimed.

With the Phase 1 decoration effects enabled (rounded corners, borders,
shadows; same workload, sorted binary from the precision-fix rebuild,
2026-10-25): **373 rendered frames** and 733 commits with the same 360 skipped —
cadence and damage behavior identical to the no-effects reference. Process CPU
rose from 1.16 s to **1.23 s (+6%)**, scene wall 0.478 → 0.498 s, compose wall
0.197 → **0.273 s (+39%)** (~0.2 ms per rendered frame; the extra cost is
fragment work for the decoration coverage on the software rasterizer), present
wall 0.102 → 0.138 s, and RSS 169,384 → **184,644 KiB (+15 MiB)** for shader
programs and element state. At these numbers the default-on decorations are
affordable on this machine, so `shadow.enabled` stays true by default; all
caveats above (single-run, directional, llvmpipe) apply to this comparison too.

A later default-config uniform-allocation change builds shadow/border uniform
arrays on the stack and allocates stored vectors only if their values change.
In paired 12-second Noctalia + 30-fps foot runs (llvmpipe, 2026-10-26), before
vs after: **373 vs 372 frames**, CPU **1.25 vs 1.20 s**, scene wall **521 vs
471 ms**, composition **276 vs 268 ms**, RSS **166,624 vs 166,480 KiB**. This
removes two temporary vector allocations per decorated window per rendered
frame when uniforms are stable; the measured CPU/scene change is directional,
not a statistically established speedup. Host-present wall also varied (56
vs 43 ms), but the uniform change is not credited for that difference.
Raw samples: local validation records, not tracked in this repository.

Blur usually belongs in **GPU textures**, with bounded cache memory and
invalidation when the background, geometry, scale, or blur parameters change.
Copying each frame into system RAM can add bandwidth and synchronization cost.
Resize springs interpolate geometry only while moving; they are off by
default. Blur remains opt-in because its measured llvmpipe cost is high.

## Tests

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --bins
python3 scripts/smoke.py
# After cargo build --release:
python3 scripts/smoke.py --release
```

The smoke test requires `foot` and a running desktop. It opens a temporary
nested Wisp instance, exercises real clients and IPC, then shuts that instance
down. It also rejects the five missing-protocol warnings and shutdown broken-pipe
errors, checks successful quit replies in an empty session, and verifies a
bounded exit with an unresponsive windowless client.
It rings a background foot client's bell to verify urgency, no focus stealing,
and urgency clearing on focus.
It does not inject input into your host session.

`cargo test` also exercises real protocol round trips over private socket pairs:
fractional scale/viewport commits, double-buffered icon metadata, cursor shape
authorization, activation/attention policy, and a separate IME provider's text,
keyboard grab, and candidate popup lifecycle. These tests need no host display.
Icon regressions cover batched destruction, metadata surviving icon destruction,
buffer replacement/sharing, invalid buffers, and immutable icon enforcement.

## Next milestones / limitations

Workspace slides are opt-in with `[layout] workspace_slide = true` and
`workspace_slide_ms = 240` (80–500). The experimental revised transition moves
outgoing/incoming windows a shorter distance and crossfades their content;
layers and cursor stay fixed. Client input is isolated until it finishes;
switching again or resizing cancels the old slide. Fmt, clippy, 90 tests,
release build and nested smoke passed for the original full-width slide; the
revised transition passed fmt, clippy, 95 tests, release build and nested
smoke; the user subsequently confirmed it looks good. No transition
performance measurements were made.
With a slide enabled, holding Alt across **Alt+2 → Alt+Return** keeps the
modifier active for both shortcuts, including while the slide runs. This fix
passed a keyboard regression and nested checks but awaits a direct-VT retest.

Both backends already compose with GLES through Smithay's per-output damage
tracker; direct mode additionally advertises renderer-importable dmabufs. This
can still run on a software GLES driver; it is **not** zero-copy scanout or
proof of an accelerated client's performance.
The existing `[layout] spring_resize = true` animates geometry changes when
tiling windows are moved or added. New toplevels can also fade in and grow
slightly into place with `[layout] window_open_fade = true` (duration set by
`window_open_fade_ms`, default 160 ms). These effects are off by default.
A nested opt-in smoke recorded 17 frames for a workspace slide and zero
additional frames in the following idle interval. Five-second single-run
nested samples with an animated foot client recorded 150 frames and 0.44 CPU
seconds with defaults versus 150 frames and 0.43 CPU seconds with animation
settings enabled; the steady-state sample **does not exercise transitions**,
so it cannot establish animation cost or smoothness. Raw data stays in local
validation records.

A single-window scratchpad can be controlled by `wispctl send-to-scratchpad`
and `wispctl toggle-scratchpad`, or via `[extra_bindings]` actions of the same
names. It hides on workspace switches and reappears floating on the current
workspace. `wisp --status-stdout` optionally streams changed workspace/window
snapshots as one JSON object per line to stdout (logs stay on stderr); a
bounded writer queue prevents a slow reader from blocking the compositor.
XDG shell and `focus_follows_mouse = false` are already supported. Full
multi-monitor support and output scaling are **not** implemented; direct mode
still drives one connector. A lock protocol exists only as an experimental,
off-by-default path, not a validated security boundary. This batch has not been
visually checked by the user or tested on direct GPU hardware; no performance
or lock security claims are made.

1. Validate direct-only `linux-dmabuf` buffer import/rendering on real hardware
   with an accelerated client. The v4/v3 feedback and rejection/fallback paths
   have protocol tests; nested mode intentionally stays `wl_shm` only. Broader
   protocol correctness tests (including resize acknowledgements) remain.
2. Layer shell for panels/launchers, presentation feedback, configurable output
   scaling/management, IME engine integration tests, and broader compatibility.
3. Hardware validation of the experimental direct backend, followed by multiple
   monitors/GPUs, hardware cursor planes, and direct scanout of client buffers.
4. Effects roadmap, each with explicit frame-time/memory budgets: **Phase 1**
   decorations (rounded corners, gradient borders, shadows) shipped; **Phase 2**
   shipped (dim-inactive on by default; Kawase blur implemented and validated
   on llvmpipe, off by default after measurement). Phase 3 smart gaps are
   implemented as opt-in. Phase 4 resize springs are implemented as opt-in,
    validated in nested tests but awaiting user visual confirmation and direct
    measurements. The experimental workspace swipe was removed after an
    unsuccessful direct touchpad test. Phase 5 current-workspace Exposé overview
    works via keyboard/IPC and pointer selection; the user confirmed the zoom
    animation visually. Experimental pinch entry was also removed after the
    direct touchpad test. Benchmarks against a minimal
   Hyprland configuration remain pending.

Currently there is one nested or direct output at compositor scale 1. Native
`wl_shm` clients are composed using GLES. `linux-dmabuf` is direct-only and
not yet hardware-validated. There is no validated XWayland desktop integration,
lock security policy, or host clipboard bridge.
An **experimental, off-by-default** `ext-session-lock-v1` server is available
with `[general] session_lock_protocol = true` (restart required). It paints a
black frame before confirming a lock, restricts scene/input to the lock
surface, checks the exact owner before honoring unlock (Smithay 0.7 calls its
handler even after `invalid_unlock`), and stays black if the locker disappears.
It requires a separate compatible locker such as swaylock; Wisp does **not**
authenticate passwords itself. The protocol tests passed, and the user has
reported successful swaylock nested lock/unlock and real-VT locking with
swaylock and Noctalia lock. The user reported no desktop flash in the real-VT
swaylock test, rejected wrong passwords (PAM `invalid credentials`) and
successful unlock with the correct password. VT-away/return, hotplug and
disconnect recovery remain unreported, so this path
**must not yet be relied on to secure an unattended session**. XWayland
remains experimental and not fully validated; `[general] xwayland = true` starts the
first opt-in XWayland/XWM path. A nested python-xlib client passed X11 window
mapping, unmanaged popup, IPC close and X11 focus checks when the host window
was active; real nested GTK clients transferred clipboard and primary text
in both X11↔Wayland directions, including live X11-to-Wayland ownership
replacement for both selections. A nested swaylock test denied one fresh
Wayland-to-X11 clipboard request while locked, and a reproducible held-offer
harness confirmed an X11-origin clipboard offer granted before locking
delivered zero bytes when its receive happened after lock confirmation.
A large in-flight X11→Wayland transfer remains a known upstream gap: Smithay
0.7's XWM has no public cancel API, so an already-started receive continues
until its source finishes; the leak is bounded to in-progress transfers.
Existing offers, pending
transfers, XWayland disconnect recovery, and direct-VT X11 behavior are not
validated. The user additionally reported working primary selection and
nested foot/xterm clipboard transfer in both directions; with xterm open,
swaylock hid it and everything returned to normal after unlock. This is a
user-observed nested check, not a lock-security guarantee. Leave it off for
daily use.
Maximize currently follows the same policy as fullscreen. Top/left interactive
resize uses the desired geometry immediately rather than waiting for the
matching client commit; slow clients may visually lag. This is a foundation
to develop and test, **not a production session or security boundary**.

MIT licensed. See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for Smithay
and example-code attribution.
