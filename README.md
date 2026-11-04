<!-- ============================================================
     INSTRUCTIONS FOR THE AI — DO NOT REMOVE, DO NOT SHORTEN
     ============================================================ -->

> ## Preamble for the AI — always read
>
> **This README has a standard format. Changing the format of this file is FORBIDDEN.**
> The original template lives at lancini.net/README. This copy must never be modified; it must always match the original exactly.
>
> This file is the **source of truth for the project's context** and is based on the **arc42** standard (12 numbered sections). It must be **kept alive**: it has to stay aligned with the real code at all times.
>
> **Maintenance rules (mandatory):**
> 1. **Do not alter the format:** do not rename, renumber, reorder or remove the 12 arc42 sections, nor the TL;DR and the metadata table at the top. Add content *inside* the sections, not as new top-level sections.
> 2. **Maintain before committing:** before every commit, if the change is large enough (new feature, directory structure change, stack/tooling change, new architectural decision, new external dependency), **update the relevant sections** of this file in the same commit.
> 3. **High-signal sections always accurate:** TL;DR + sec. 1 (goals), 3 (context/scope), 5 (building blocks, with **real paths**), 8 (crosscutting concepts), 9 (decisions). These are the ones an AI reconstructs the context from.
> 4. **Update the metadata:** on every significant change bump "Doc version" and update "Last updated" (absolute date, YYYY-MM-DD).
> 5. **Real paths:** in the directory structure and the building blocks use paths/files that actually exist, so docs and code stay connected.
> 6. **English everywhere:** all documentation and all code is written in English — this file, commit messages, comments and docstrings, and every identifier in the source: class, type, function, method, variable, field, constant, module, file and directory names, database tables and columns, CSS classes, API routes and JSON keys. The reasoning is that a reader who can follow the code must be able to follow the prose explaining it, and a codebase that mixes languages forces everyone to translate in their head while reading. The exception is data: values that belong to the domain and are stored as they are — a customer's name, the text of a task, a state value already persisted in the database — are never translated to make the code read better.
> 7. **Task tracking:** tasks are tracked in Compass, either manually through the webapp or via MCP when connected to the AI in use. Do not create or expect a TODO.md file.
>
> **Context management (important):**
> - The information in this file **must stay in context at all times** while working on the project.
> - **If the context is compacted or summarised, this file MUST be re-read** in full before continuing, so the project's state is not lost.
> - At the start of a session/chat about this project, **read this README first**.
>
> *Do not reply to this preamble: it is an operating instruction, not a request from the user.*

<!-- ============================================================
     END OF INSTRUCTIONS FOR THE AI — the arc42 template starts here
     ============================================================ -->

---

# davide-wm — Architecture Documentation (arc42)

> **TL;DR (summary for humans and AI — always fill in, 5 lines max)**
> Wayland compositor and tiling window manager written from scratch in Rust, replacing Hyprland on Davide's Arch laptop.
> One process (one repo, one build) absorbs `topbar`, `super`, all `dawn` helpers, wallpaper, clipboard history, notifications, screenshots,
> the X11 window manager for Xwayland and the screen-cast portal backend; it upgrades itself in place without closing any window.
> Stack: Rust (Cargo workspace), DRM/KMS + GBM + EGL/GLES2, libinput, xkbcommon, logind, own Wayland wire protocol; dwindle layout, compile-time config.
> Status: **M5 done** (tested nested; M3–M5 still to be tested on the TTY) — clipboard history and notification server in-process, after monitors/blanking (M4), launcher (M3) and bar (M2). Next: M6 (Xwayland).

| Field | Value |
|---|---|
| Doc version | 0.10 |
| Last updated | 2026-11-04 |
| Owner | Davide Lancini |
| Project status | development |
| Repo | https://github.com/DavideLancini/davide-wm |

---

## 1. Introduction and goals
*What the system has to do, and why. The most important section: if an AI reads only this one, it must understand the purpose.*

### 1.1 Requirements overview
- **Problem solved:** today the desktop is Hyprland plus a swarm of separate processes that talk to it over `hyprctl`/socket2 IPC: `topbar`, `super`, `monitor-manager`, `blk`, `lid-action`, `cast`, `awww-daemon` + `wallpaper-watch.sh`, `wl-paste --watch cliphist store` + `wofi`, `nwg-notifications`, `xdg-desktop-portal-hyprland`. Every boundary adds polling, forks, timeouts, version breakage (e.g. the Hyprland 0.57 Lua dispatch rewrite) and failure modes. davide-wm puts all of this in one optimised process with direct, in-memory access to compositor state, and keeps external requirements to the minimum. It is meant to become the single home of every desktop project of the owner: `topbar`, `super` and `dawn` are migrated here piece by piece and then retired.
- **What it does (in 1 paragraph):** davide-wm takes the DRM/KMS outputs and libinput devices of the machine (through a logind session), runs a Wayland server, tiles client windows with a dwindle layout over numbered workspaces, and composites them with GLES2 (direct scanout when possible). Inside the same process it draws the top bar and the launcher overlay, manages monitors (hotplug rules, DPMS, idle blanking, lid), shows the wallpaper, keeps the clipboard history, acts as the freedesktop notification server, manages Xwayland windows as their X11 WM, and serves screen casting to browsers through its own xdg-desktop-portal backend and a PipeWire stream.
- **Main features:**
  - Compositor core: DRM/KMS multi-monitor with hotplug, libinput (keyboard `it`, touchpad natural scroll, flat accel), xkbcommon, hardware cursor, damage tracking, direct scanout for fullscreen.
  - Window management: dwindle tiling, maximize, fullscreen, floating overlays, workspaces 1–41 plus `chat` and the special `resize`, all keybinds currently in `~/.config/hypr/hyprland.lua`, window rules, urgency (system bell, xdg-activation).
  - Bar (from `topbar`): one bar per output, every current widget and click action, workspace urgency.
  - Launcher (from `super`): tap-Super overlay with `.desktop` apps, `~/2Bookmarks`, `=` calculator.
  - Clipboard: selection + primary selection, history persisted across reboots and picker (replaces cliphist/wl-paste/wofi), data-control for external tools.
  - Notifications: `org.freedesktop.Notifications` server with popups and a panel (replaces nwg-notifications).
  - Monitor management (from `dawn`): per-monitor rules, DPMS, idle blanking, black screen (`blk`), lid handling, headless outputs for casting and the wallpaper (replaces awww).
  - Xwayland: in-process X11 window manager; only the `Xwayland` binary stays external.
  - Screen sharing: own `org.freedesktop.impl.portal.ScreenCast` backend + PipeWire video stream.
  - Screenshots: own taker (region, window or output → file + clipboard), no annotations (replaces grim/slurp/swappy).
  - Live upgrade: a new build replaces the running binary in place; clients, windows, Xwayland and outputs survive.

### 1.2 Quality goals (top 3-5)
*The architectural qualities that take priority, in order. They drive the decisions.*

| # | Quality | Rationale / scenario |
|---|---------|----------------------|
| 1 | Reliability | A compositor crash or freeze takes the whole session down with it. Nothing may block the main loop; every wait has a timeout; panics in non-core features (bar widgets, launcher, notifications) must not kill the compositor. |
| 2 | Responsiveness | Input to photon within one refresh; no dropped frames at 120 Hz on the MSI monitor; render only on damage. |
| 3 | Low footprint | Idle CPU and RSS lower than today's Hyprland + topbar + super + helpers combined; no polling where an event source exists. |
| 4 | Minimal external requirements | Every external daemon/tool replaced unless it is a system service clients themselves depend on (PipeWire, xdg-desktop-portal, D-Bus, logind) or a full program (Xwayland). |
| 5 | Maintainability | Single maintainer working with AI: small explicit modules, compile-time config, no frameworks hiding behaviour. |

### 1.3 Stakeholders
| Role | Who | Expectations |
|------|-----|--------------|
| End user | Davide Lancini (only user, one laptop) | Same workflow as the current Hyprland setup, faster and with fewer moving parts; never lose the session. |
| Developer | Davide Lancini, with AI assistance | Code readable without a compositor framework; can develop nested inside the running Hyprland. |
| Owner | Davide Lancini | Fewer packages installed; one process to build, install and debug. |

---

## 2. Constraints
*What is NOT negotiable: it narrows the choices. Helps the AI not to propose solutions that are out of scope.*

- **Technical:**
  - Rust only. Written from scratch: no compositor framework (no smithay, no wlroots). Thin bindings to system libraries are allowed; in practice they are hand-written FFI (ADR-020) and the Wayland protocol is our own (ADR-019).
  - Target: Arch Linux, Huawei MateBook KLVL-WXX9, AMD Renoir (amdgpu, Mesa), eDP-1 2160x1440@60 scale 1; external MSI MAG 273Q 2560x1440@120 (DP-1, 8-bit) and a Hisense TV. Scale 1 everywhere, no HDR, no VRR.
  - No lock screen (blanking with `blk` is enough). No animations, borders, gaps, rounding, blur or shadows.
  - Configuration is compile-time `const`s, like `topbar` and `super`; no config file, no scripting language.
  - External requirements allowed: Linux DRM/KMS, Mesa (GBM, EGL, GLES2), libinput, libxkbcommon, systemd-logind, D-Bus session bus, PipeWire (libpipewire), xdg-desktop-portal (frontend) + xdg-desktop-portal-gtk (file chooser only), the `Xwayland` binary. Anything else needs an ADR.
- **Organisational:** one repository and one binary for everything, always built together (Cargo workspace, ADR-015); the other desktop projects are migrated here and retired. Single maintainer; personal project, no deadline. Hyprland stays installed and in use until davide-wm covers the daily workflow (see milestones, sec. 4).
- **Conventions:** commits in English, imperative, concise. All code, comments, docstrings and identifiers in English. Tasks tracked in Compass, no TODO.md.

---

## 3. Context and scope
*The system's boundary: what is inside, what is outside, who it talks to. High value for the AI.*

### 3.1 Business context
*External actors (users, other systems) and what they exchange with the system.*

```
                 keyboard/touchpad/lid            displays (eDP-1, DP-1, TV, headless)
                        |                                   ^
                        v                                   |
[logind] <--D-Bus--> [ davide-wm: compositor + WM + bar + launcher + clipboard + ]
                     [ notifications + monitors + wallpaper + XWM + portal impl ]
                        ^        ^            ^                 |
          Wayland socket|  X11   |    D-Bus   |                 | PipeWire video stream
                        |        |            |                 v
              [Wayland clients] [Xwayland] [apps sending      [PipeWire] <-- [browser]
               foot, chromium,   (spawned)   notifications,       ^              |
               code, thunar...]              MPRIS, BlueZ,         |   ScreenCast portal
                                             mega, ...]     [xdg-desktop-portal] <-+
```

| External partner | Input | Output |
|------------------|-------|--------|
| User (keyboard, touchpad, lid) | key/pointer/switch events via libinput | windows, bar, launcher, popups on screen |
| Wayland clients | protocol requests, shm/dmabuf buffers | input events, configure, frame callbacks |
| Xwayland (child process) | X11 window events, wl_surfaces via xwayland-shell | X11 WM decisions (map, configure, focus, stacking) |
| systemd-logind | device fds (DRM, evdev), session active/pause, lid | TakeDevice/ReleaseDevice, VT switch requests |
| Applications sending notifications | `org.freedesktop.Notifications.Notify` | popups, panel, workspace urgency |
| xdg-desktop-portal frontend | ScreenCast `CreateSession`/`SelectSources`/`Start` | PipeWire node id of the cast stream |
| PipeWire | stream negotiation | video frames (dmabuf or shm) |
| PulseAudio protocol (pipewire-pulse), BlueZ, MPRIS, mega-cmd-server, Spotify Web API, sysfs/procfs | bar data | mute toggles, bar text |
| Shell scripts / CLI | commands on the IPC socket (`davide-wm msg ...`) | replies, event stream |

### 3.2 Technical context
*Interfaces, protocols, channels (HTTP, gRPC, files, queues, etc.).*

| Interface | Protocol / format | Notes |
|-----------|-------------------|-------|
| Wayland core | `wl_compositor`, `wl_subcompositor`, `wl_shm`, `wl_seat`, `wl_output`, `wl_data_device_manager` | Baseline for every client. |
| Windows | `xdg_wm_base` (toplevel, popup), `xdg-decoration` (always server-side, drawn as nothing), `xdg-activation-v1`, `xdg-system-bell-v1` | Activation and bell drive focus requests and workspace urgency. |
| Buffers and timing | `linux-dmabuf-v1`, `wp_viewporter`, `wp_presentation`, `xdg-output` | dmabuf is mandatory for GPU clients (Chromium, VS Code). |
| Input extras | `wp_cursor_shape_v1`, `zwp_idle_inhibit_manager_v1`, `wp_primary_selection`, `zwp_pointer_constraints_v1` + `zwp_relative_pointer_v1` (later) | Idle inhibit feeds the idle blanking. |
| Clipboard for external tools | `ext-data-control-v1` (+ `zwlr_data_control` if needed) | Lets `wl-copy`/`wl-paste` (used by `tts --clipboard`) keep working. |
| Screenshots for external tools | `ext-image-copy-capture-v1` (optional, later) | Only if an external tool still needs it; screenshots are built in. |
| Xwayland | X11 via `x11rb` on a socketpair; `xwayland-shell-v1` | Compositor spawns `Xwayland -rootless -wm <fd>`, sets `DISPLAY`. |
| Session | logind over D-Bus (`TakeControl`, `TakeDevice`, `PauseDevice`, `ResumeDevice`, `SwitchTo`) | No libseat/seatd. |
| Devices | DRM ioctls (atomic KMS), GBM, EGL + GLES2; libinput (path backend, fds from logind); netlink uevents for hotplug | |
| Notifications | D-Bus `org.freedesktop.Notifications` (server side) *(planned)* | Own D-Bus implementation, extended from `topbar/src/dbus.rs`. |
| Screen cast | D-Bus `org.freedesktop.impl.portal.ScreenCast` (+ `Screenshot`), PipeWire stream via libpipewire | `davide-wm.portal` file + `davide-wm-portals.conf` installed under `/usr/share/xdg-desktop-portal/`. |
| IPC | Unix socket `$XDG_RUNTIME_DIR/davide-wm-<wayland socket>.sock` (path exported to children as `DAVIDE_WM_SOCKET`): one text command per connection (`workspace`, `exec`, `close`, `windows`, `focus-app`, `upgrade`, `exit`; test helpers `pointer`, `click`, `focused`), one reply; event stream *(planned)* | For scripts that remain outside; replaces `hyprctl`/socket2. Client: `davide-wm msg <command>`. |
| Signals | `signalfd`: TERM/INT (clean exit), USR1/USR2 kept for compatibility (bar toggle/refresh) | |

---

## 4. Solution strategy
*The fundamental decisions in brief: key technologies, patterns, approach. The condensed "why".*

- **Main stack:** Rust (stable). The only crate dependency is `libc`. Own Wayland wire protocol (`crates/wire`, tables generated from the vendored XML in `protocols/`, ADR-019); hand-written FFI to libEGL, libGLESv2, libgbm and libxkbcommon (`crates/sys`, ADR-020); own epoll event loop. Planned: libinput and libpipewire through the same kind of FFI, `x11rb` or own X11 code for the XWM, `fontdue` for text (already used by topbar and super).
- **Architectural patterns:** modular monolith, single-threaded epoll event loop owning all state, with every potentially blocking operation (HTTP, process exec with output, file decoding, D-Bus calls with replies) moved to worker threads that post results back as events. Backends behind a small trait: `drm` (real hardware), `nested` (runs as a window inside an existing Wayland compositor, for development), `headless` (tests and cast outputs).
- **Key technology decisions:** from scratch instead of smithay/wlroots (ADR-001); GLES2 renderer with direct scanout instead of software compositing (ADR-002); logind directly over D-Bus instead of libseat (ADR-003); compile-time config (ADR-004); topbar/super/dawn code ported as in-process modules (ADR-005); own XWM (ADR-008); own portal backend + PipeWire (ADR-009).
- **How it meets the quality goals (sec. 1.2):** single process and in-memory state remove IPC, forks and polling (footprint, responsiveness); the non-blocking rule and per-feature panic isolation protect the session (reliability); every replaced daemon is one package less (minimal requirements); explicit modules with no framework and compile-time config keep it readable (maintainability).
- **Milestones (each one usable before the next starts):**
  - **M0 — Nested skeleton (done 2026-11-04):** nested backend (dmabuf to the host), own wire protocol, `wl_compositor`/`wl_subcompositor`/`wl_shm`/`wl_seat`/`wl_output`/`xdg_wm_base` (toplevels, basic popups)/xdg-decoration (always server-side)/`wl_data_device_manager` stub, GLES composition, dwindle tiling, keyboard (xkb `it`) and pointer, keybinds, spawn `foot`.
  - **M1 — Daily-driver core on a TTY:** *done in nested mode (2026-11-04):* workspaces 1–41 + `chat` + special `resize`, keybind parity with `hyprland.lua`, move/swap windows, maximize/fullscreen, window rules, clipboard + primary selection + ext-data-control, xdg-activation and system bell (urgency), idle-inhibit, linux-dmabuf import, SIGBUS guard, IPC socket, compiled-in wallpaper. *Tested on the TTY (2026-11-04):* logind session (own D-Bus), atomic KMS with raw ioctls, libinput (flat accel, tap, natural scroll on touchpads), DRM hotplug via netlink uevents, VT switching, Ctrl+Alt+Backspace emergency exit. *Remaining:* hardware cursor, damage tracking, done since: synchronised subsurfaces. One output at a time until M4 (eDP-1 first).
  - **M1b — Live upgrade (done 2026-11-04, tested nested; DRM path untested):** `davide-wm msg upgrade`, Super+Shift+C or `./run.sh` re-exec the binary on disk with state and fd handover (ADR-014). About 170 ms in a debug build with Chromium and foot open; windows, clipboard, workspaces and the wallpaper survive.
  - **M2 — Bar (done 2026-11-04):** `topbar` ported to `crates/bar`, running on its own thread (ADR-029); the compositor reserves its strip, draws its frames, routes clicks and feeds it workspaces, urgency and windows. Main+Ctrl+Alt+Shift+F12 hides/shows it. Topbar's systemd service is retired at switch-over.
  - **M3 — Launcher (done 2026-11-04):** `super` ported to `crates/launcher` (desktop entries, `~/2Bookmarks`, `=` calculator). Tapping the main modifier (Super; Alt when nested) toggles it; while open it has the keyboard, Esc or a click outside closes it, Enter launches through the compositor's spawn.
  - **M4 — Monitors (done 2026-11-04, tested nested; the DRM paths still need a TTY test with the MSI):**
    - *Inventory of `dawn`:* `monitor-manager` (2200 lines: hotplug via socket2, EDID classification, layouts MSI-only / Hisense mirror / other above eDP / laptop-only, eDP-1 disabled under the MSI, DP link-training check in the kernel log, watchdog, idle blanking from `/dev/input`, screensaver state 1m/5m/30m/OFF with auto/manual source, waybar JSON), `blackout-core` + `blk` (DPMS off, or a black `foot` window over the MSI, until the first input, 700 ms grace, guard loop against the MSI re-handshake), `lid-action` / `lid-inhibit-toggle` / `lid-status` (a `systemd-inhibit` process in a pidfile; eDP-1 off on close while inhibited). `cast` is left for M7; the other `dawn` crates are unrelated to the compositor.
    - *Ported:* several outputs at once (ADR-033): an exclusive monitor (`config::OUTPUT_EXCLUSIVE`, the MSI) alone at its highest refresh, else the laptop panel with other monitors mirroring it; the link check after lighting the MSI (ADR-036); DPMS per output; `blk` as an in-process black screen (black frames on the MSI, DPMS off elsewhere; ADR-034); the idle screensaver with its auto/manual policy, honouring idle inhibitors on screen; the lid inhibitor as a logind fd (ADR-035); with the lid closed the panel stays dark and an external monitor takes over. The bar's blk, screensaver and lid widgets now talk to the compositor. IPC: `blank`, `wake`, `screensaver [cycle|on|off|auto|<secs>]`, `lid-inhibit [toggle]`, `outputs`.
    - *Not ported:* HDR / 10-bit on the MSI (needs a colour pipeline), the watchdog (the compositor knows what it lit), the guard loop (the MSI is never powered down).
  - **M5 — Clipboard history + notifications (done 2026-11-04, tested nested with a private session bus):**
    - *Clipboard (ADR-037):* every new clipboard selection is read from its owner on a worker thread (text preferred, else an image; entries marked `x-kde-passwordManagerHint` skipped), kept newest first without duplicates (`config::CLIPBOARD_HISTORY`, 16 MiB per entry) and saved to `$XDG_STATE_HOME/davide-wm/clipboard` (mode 0600). Main+P opens the picker in the launcher box: type to filter, Enter makes the entry the clipboard (the compositor serves it), Delete removes it. The clipboard survives the program it was copied from.
    - *Notifications (ADR-038):* `org.freedesktop.Notifications` on the session bus (Notify, CloseNotification, GetCapabilities `actions body persistence`, GetServerInformation; NotificationClosed, ActionInvoked). Popups at the top right below the bar, newest on top, at most `NOTIFICATION_POPUPS`; 5 s unless the app says otherwise, critical ones stay. Left click runs the default action and focuses the app's window, other buttons dismiss. Main+N or the bar's bell opens the list in the launcher box: Enter focuses and removes, Delete removes, the first row clears all. The workspace of the app that notified is marked urgent, as before.
    - *Bar:* the nwg-notifications count and the bus monitor are gone; the compositor sends the count and each new notification.
    - *Not done:* icons and images, body markup, inline replies, do-not-disturb. IPC for tests: `clipboard`, `notifications`, `key` with several codes (a chord).
  - **M6 — Xwayland:** XWM in-process.
  - **M7 — Screen sharing:** portal backend, PipeWire stream (outputs and windows), headless outputs for `cast`, screenshot taker.
  - **Switch-over:** remove Hyprland, xdg-desktop-portal-hyprland/-wlr, awww, cliphist, wofi, nwg-notifications, grim, slurp, swappy, nm-applet, the topbar and super binaries and the absorbed dawn binaries; archive the `topbar`, `super` and `dawn` repos.
  - **Afterwards:** keep migrating the remaining `dawn` crates and helper scripts the bar calls (`cpu-cycle`, `brightness`, `set-timer.sh`, `bt-headphones-battery`, ...).

---

## 5. Building block view (structure)
*Static decomposition of the system. THIS is where the real paths of folders/modules go — it connects docs and code.*

### 5.1 Level 1 — the system as a whole
*A box diagram of the main modules/components and their responsibilities.*

Cargo workspace (ADR-015): one library crate per building block under `crates/`, one binary crate that wires them together. Rows marked *(planned)* do not exist yet; the Wayland server currently lives inside the binary crate (`crates/davide-wm/src/server/`) and moves to its own crate when it grows. Update the table as crates are created.

| Building block | Responsibility | Path |
|----------------|----------------|------|
| Binary / event loop | startup, backend selection (`--nested`/`--drm`, default by `WAYLAND_DISPLAY`), epoll loop, signals, socket, keybind dispatch, spawning, `outputs` debug command; later crash handling, live upgrade | `crates/davide-wm/src/main.rs`, `event_loop.rs`, `keyboard.rs`, `worker.rs` |
| Wire protocol | message codec, connections with fd passing, interface tables and opcode constants generated at build time from `protocols/*.xml` | `crates/wire/` |
| System FFI | hand-written bindings: EGL, GLES, GBM, xkbcommon | `crates/sys/` |
| JPEG decoder | baseline JPEG (Huffman, any subsampling, restart markers) for the compiled-in wallpaper; box-filter halving | `crates/jpeg/` |
| Config | all settings as `const`s: keybinds, colors, keyboard layout, render node; later window and monitor rules, fonts, paths | `crates/config/` |
| State / handover | `Persist` trait and binary encoder (`persist_struct!` lists every field), fd handover across exec; implementations live next to each type (`wm`, `wire`, `render`, `dbus`, `crates/davide-wm/src/server/persist.rs`, backends); orchestration (`encode`, `decode`, `upgrade`, `resume`) in `main.rs` | `crates/state/` |
| Backends | `nested` (client of the host compositor) and `drm`: `logind.rs` (session, TakeDevice, VT switch), `kms.rs` (atomic modesetting, scanout buffers, page flips), `input.rs` (libinput via logind); `headless` *(planned)* | `crates/davide-wm/src/backend/` |
| Renderer | EGL on GBM (surfaceless GLES 3 context), render targets exported as dmabufs, shm texture upload, solid/textured quads, scissor clip; later dmabuf import, damage, scanout planes, text | `crates/render/` |
| Wayland server | clients and object maps, globals, protocol handlers (core, seat/output/focus, xdg-shell), surface tree, rendering of the scene | `crates/davide-wm/src/server/` |
| Window manager | dwindle tree, directional neighbours, `Rect`; later workspaces, rules, fullscreen/maximize, urgency | `crates/wm/` |
| Outputs | monitor policy and EDID, several CRTCs with mirroring, DPMS, blanking, idle screensaver, lid; headless outputs *(M7)* | `crates/davide-wm/src/backend/{kms,outputs}.rs`, `display.rs` |
| Bar | topbar's widgets, layout and drawing on a dedicated thread with its own epoll loop (`event_loop.rs`, `zones.rs`); talks to the compositor only through `ToBar`/`FromBar` messages and an eventfd | `crates/bar/` |
| Launcher | desktop-entry and bookmark scan, filtering, calculator, drawing of the box (`apps.rs`, `calc.rs`, `draw.rs`); the compositor owns the overlay (`LauncherView` in `crates/davide-wm/src/server/mod.rs`) and the keyboard grab | `crates/launcher/` |
| Clipboard | selections, persisted history, picker, data-control *(planned)* | `crates/clipboard/` |
| Notifications | D-Bus notification server, popups, panel | `crates/notifications/` |
| XWM | Xwayland process + X11 window management *(planned)* | `crates/xwayland/` |
| Screen cast / screenshots | portal backend, PipeWire streams, screenshot taker *(planned)* | `crates/screencast/` |
| D-Bus | own minimal client: SASL EXTERNAL, little-endian marshalling of every type, calls with timeouts, signals, fd passing; server side: name requests, method returns, errors, signals | `crates/dbus/` |
| IPC | Unix socket commands, `davide-wm msg` CLI | `crates/davide-wm/src/ipc.rs` |
| Process exec | detached spawn (`setsid`, close fds), helpers with timeouts *(planned)* | `crates/exec/` |

### 5.2 Directory structure
```
davide-wm/
├── Cargo.toml              # workspace; release profile
├── README.md               # this file
├── protocols/              # vendored protocol XML (wayland, xdg-shell, xdg-decoration, linux-dmabuf)
└── crates/
    ├── davide-wm/          # binary
    │   └── src/
    │       ├── main.rs         # startup, socket, epoll loop, keybinds, spawn
    │       ├── event_loop.rs   # epoll + signalfd
    │       ├── keyboard.rs     # xkb keymap/state, keymap memfd
    │       ├── backend/        # mod.rs (events, Drm), nested.rs, logind.rs, kms.rs, input.rs
    │       ├── ipc.rs          # command socket and `davide-wm msg`
    │       ├── worker.rs       # background jobs (thread + eventfd)
    │       └── server/         # mod.rs (state, clients, render), compositor.rs (core, shm, SIGBUS guard), seat.rs,
    │                           # xdg.rs, workspaces.rs, selection.rs, dmabuf.rs, misc.rs (activation, bell, idle-inhibit)
    ├── wire/               # build.rs generates interface tables from protocols/
    ├── sys/                # egl.rs, gl.rs, gbm.rs, xkb.rs
    ├── render/
    ├── bar/                # ported topbar: modules/, draw.rs, pulse.rs, dbus.rs (own D-Bus client, to merge with crates/dbus), assets/ font
    ├── dbus/
    ├── jpeg/
    ├── launcher/           # ported super: apps.rs, calc.rs, draw.rs, assets/ font
    ├── wm/
    └── config/
```
`run.sh` builds the release binary and live-upgrades every running instance. `dist/davide-wm-session.target` binds the session to `graphical-session.target` (install it in `~/.config/systemd/user/`). Planned additions: `assets/`, `tools/` and the crates marked *(planned)* in sec. 5.1.

### 5.3 Level 2 — detail of the relevant building blocks
*Expand only the blocks that are not obvious. (Repeat for each one.)*

**Bar (`crates/bar/`)** — Ported from `~/0Projects/topbar` with its compositor-independent code almost unchanged (`draw.rs`, `icons.rs`, the font asset, `pulse.rs`, `dbus.rs`, `notify.rs`, `json.rs`, every sysfs/procfs widget). Removed: the layer-shell client, Hyprland IPC, single-instance and signal handling, urgency tracking. The compositor sends `ToBar::Width`/`Workspaces`/`Clients`/`Click`; the bar answers with `FromBar::Frame` (ARGB pixels, uploaded as a texture), `Workspace` (switch), `FocusApp` (Spotify) and `Notified` (a desktop notification came from an app on that workspace, which turns urgent until visited). Blocking work (helpers with timeouts, MEGA, Spotify HTTP) stays on the bar thread; a panic restarts its loop after 1 s.

**Launcher (`crates/launcher/`)** — `apps.rs`, `calc.rs` and `draw.rs` come from `~/0Projects/super` almost unchanged; the layer-shell client, singleton and idle watchdog are gone (the compositor always keeps Esc, the tap and a click outside as ways out). It runs on the main thread: scanning entries on open takes a few milliseconds. Spawned programs get every fd above stderr closed (`close_range`, super's ADR-8).

**Outputs (`backend/outputs.rs`, `backend/kms.rs`, `display.rs`)** — Absorbs `~/0Projects/dawn/crates/` `monitor-manager` (hotplug rules, workspace placement, eDP-1 watchdog, idle blanking from input activity), `blk`/`blackout-core` (black screen per output; the MSI loses signal on DPMS, so it gets a black plane instead of DPMS off), `lid-action`, and `cast` (headless outputs). Idle blanking honours `idle-inhibit`.

**XWM (`crates/xwayland/`)** — Spawns `Xwayland` on demand with a WM socketpair, runs an X11 window manager with `x11rb` (EWMH/ICCCM subset: map/configure, focus, transient/override-redirect, `_NET_WM_STATE` fullscreen, selections bridged to the Wayland clipboard) and pairs X11 windows with `wl_surface`s through `xwayland-shell-v1`.

**Screen cast (`crates/screencast/`)** — Owns `org.freedesktop.impl.portal.desktop.davidewm` on the session bus and implements `ScreenCast` (monitor and window sources, chosen through a picker drawn by the compositor) and `Screenshot`. Frames are copied (or exported as dmabuf) into a PipeWire stream created with libpipewire. The screenshot taker (region, window or output; saved to file and copied to the clipboard; no annotations) replaces `grim`/`slurp`/`swappy`.

---

## 6. Runtime view
*Important dynamic scenarios: how the components work together at runtime. Only the key flows.*

### Scenario: "Startup from a TTY"
1. User runs `davide-wm` on a TTY; it opens a logind session (`TakeControl`), takes the DRM device and input devices with `TakeDevice`.
2. Enumerates connectors, applies monitor rules from `crates/config/`, sets modes with an atomic commit, initialises GBM/EGL per GPU.
3. Creates the Wayland socket, exports `WAYLAND_DISPLAY`/`DISPLAY` to the systemd user manager and D-Bus activation environment, starts `graphical-session.target`.
4. Registers D-Bus names (notifications, portal backend), opens the IPC socket, loads the persisted clipboard history, spawns autostart programs (`foot --server`).

### Scenario: "Live upgrade"
1. `run.sh` installs the new binary and sends `davide-wm msg upgrade` (or a keybind triggers it).
2. The running process finishes the current frame, stops reading client sockets, serialises its whole state (protocol objects of every client, windows, workspaces, outputs, seat, selections, bar/notification/clipboard state) into a versioned blob in a `memfd`, and clears `O_CLOEXEC` on the fds to keep: Wayland listening socket and client connections, shm/dmabuf buffers, DRM, input devices, logind D-Bus connection, Xwayland WM socket, PipeWire, IPC socket.
3. It `execve`s the new binary with `--resume <memfd>`: same PID, so Xwayland and spawned children stay its children and the logind session stays valid.
4. The new process rebuilds state from the blob, re-creates the GL context and re-imports textures from the kept buffers, and resumes reading clients. Clients see at most one delayed frame.
5. If the blob version is unknown or restoring fails, it `execve`s the previous binary (kept as `davide-wm.prev`) with the same blob.

### Scenario: "Key press to frame"
1. libinput fd readable → event processed by xkbcommon.
2. Keybind table checked first; if matched, the WM action runs (e.g. switch workspace); otherwise the key goes to the focused client.
3. Damaged outputs are scheduled; on the next vblank the renderer composites only damage (or scans out a fullscreen client buffer directly) and sends frame callbacks.

### Scenario: "Browser starts a screen cast"
1. Browser calls the xdg-desktop-portal frontend, which forwards `CreateSession`/`SelectSources`/`Start` to davide-wm's portal backend.
2. davide-wm shows its source picker, creates a PipeWire stream for the chosen output or window and returns its node id.
3. After every rendered frame of that source, the frame is copied (or dmabuf-exported) into the stream until the session is closed.

### Scenario: "Blocking work"
1. A bar widget needs a slow result (Spotify HTTP, helper process, MEGA).
2. The request goes to a worker thread with a timeout; the main loop keeps running and the widget keeps its old value.
3. The result comes back through a channel watched by the event loop and triggers a redraw only if the text or colour changed.

---

## 7. Deployment view
*Where the software runs: infrastructure, environments, building block → node mapping.*

| Environment | Infrastructure | Notes |
|-------------|----------------|-------|
| dev | nested backend inside the running Hyprland session (`cargo run -p davide-wm`, then `WAYLAND_DISPLAY=<socket printed at startup> foot`); headless backend for automated tests later | Main modifier is Alt when nested (Super belongs to Hyprland): Alt+T terminal, Alt+Q close, Alt+arrows focus, Alt+Shift+E exit. No risk to the working session; DRM tests on a second TTY. |
| prod | the laptop, started from a TTY login (`davide-wm` to try it, `davide-wm --session` as the real session: exports the environment to systemd/D-Bus and starts `davide-wm-session.target`; Super is the main modifier) | Hyprland stays as fallback until switch-over. Ctrl+Alt+F1…F12 switch VT, Ctrl+Alt+Backspace exits. `davide-wm outputs` lists connectors and the one that would be driven. |

- **Build/CI-CD:** `cargo build --release` (opt-level, LTO, strip, panic handling decided in ADR-006); `run.sh` installs the binary to `~/7Scripts/davide-wm` and the portal/unit files from `dist/`. No CI.
- **Configuration & secrets:** configuration compiled in (`crates/config/`). Spotify client credentials are currently compiled into topbar (known debt, sec. 11); the refresh token stays in `~/.local/state/topbar/spotify-token` until moved to `~/.local/state/davide-wm/`.

---

## 8. Crosscutting concepts
*Themes that run through the whole system. High value: it explains the project's implicit rules.*

- **Domain model / main entities:** `Output` (connector, mode, position, DPMS state, workspaces), `Workspace` (id 1–41, `chat`, special `resize`; dwindle tree; urgent flag), `Window` (Wayland toplevel or X11 window; surface tree; rules applied), `Seat` (keyboard focus, pointer, selections), `Notification`, `ClipboardEntry`, `CastSession`.
- **Persistence:** none for compositor state across reboots (live upgrade passes it in memory). Clipboard history persisted to `~/.local/state/davide-wm/clipboard/` with a size cap; entries offered with a sensitive hint (`x-kde-passwordManagerHint: secret`) are never stored. Bar caches as in topbar.
- **Handover-ready state (live upgrade):** all long-lived state lives in plain owned structs (ids/indices, no raw pointers, no closures) that can be serialised; every kept fd is registered in one fd table; anything not serialisable (GL objects, caches) must be rebuildable from the serialised state. Serialisation format is versioned; a new field needs a default for old blobs.
- **Security & auth:** clients are trusted (single user). data-control and screen capture are privileged: data-control open to any client (needed by `wl-copy`/`wl-paste`), screen capture only through the portal (user picks the source). IPC socket is `0600` in `$XDG_RUNTIME_DIR`. Spawned processes never inherit compositor fds (close everything above stderr, `O_CLOEXEC` everywhere — super ADR-8).
- **Error handling & logging:** the main loop never blocks: every wait has a timeout (topbar ADR-052 rule: 2× worst measured case, with per-category minimums) and anything longer than a few milliseconds runs on a worker thread. Non-core features run behind `catch_unwind` and are disabled on panic instead of killing the session. On fatal errors the DRM master and VT are released before exit. Logs go to stderr → journald, no log files.
- **Internationalisation / configuration / feature flags:** English UI strings; all configuration as `const`s in `crates/config/`; features enabled at compile time only if needed (no runtime flags).
- **Testing:** unit tests for pure logic (dwindle tree, rules, calc, `.desktop` parsing, D-Bus marshalling); integration tests on the headless backend with real Wayland clients; manual testing nested, then on a TTY. Resource usage measured against a Hyprland baseline (sec. 10).
- **Input focus safety:** an overlay that grabs the keyboard (launcher, picker) must always have a way out (ESC, timeout, focus loss) — super ADR-7 locked the whole session once.

---

## 9. Architectural decisions (ADR)
*The important choices with their reasons. One register line, plus full ADRs below where needed.*

| # | Decision | Status | Date | Rationale (brief) |
|---|----------|--------|------|-------------------|
| 1 | Write the compositor from scratch (no smithay, no wlroots) | accepted | 2026-11-04 | Full control and understanding, same raw-bindings approach as topbar/super; accepted cost: much larger effort. |
| 2 | GLES2 renderer with direct scanout | proposed | 2026-11-04 | GPU clients send dmabufs; software compositing of 2160x1440 + 2560x1440@120 would cost too much CPU. |
| 3 | logind over own D-Bus instead of libseat | accepted | 2026-11-04 | One dependency less; D-Bus code is needed anyway. Calls are synchronous with a 500 ms timeout because libinput asks for device fds from inside its own calls. |
| 4 | Compile-time configuration in `crates/config/` | proposed | 2026-11-04 | Same as topbar ADR-001 and super; zero parsing, zero runtime errors from config. |
| 5 | Absorb topbar, super and dawn helpers as in-process modules | accepted | 2026-11-04 | Removes IPC, polling, forks and version breakage. |
| 6 | Single-threaded main loop + worker threads for blocking work | proposed | 2026-11-04 | Simple ownership of state; a blocked main loop freezes the screen. |
| 7 | Dwindle tiling as the only tiling layout | accepted | 2026-11-04 | Same behaviour as today's Hyprland setup. |
| 8 | Own X11 window manager for Xwayland (no xwayland-satellite) | accepted | 2026-11-04 | Minimal external requirements; only the Xwayland binary stays. |
| 9 | Own xdg-desktop-portal backend + libpipewire stream for screen sharing | accepted | 2026-11-04 | Replaces xdg-desktop-portal-hyprland/-wlr; frontend and PipeWire stay because browsers talk to them. |
| 10 | Own clipboard history and notification server | accepted | 2026-11-04 | Replaces cliphist, wl-paste, wofi and nwg-notifications. |
| 11 | No lock screen | accepted | 2026-11-04 | Blanking with blk is enough. |
| 12 | Development with a nested backend inside Hyprland | proposed | 2026-11-04 | Develop without losing the working session. |
| 13 | IPC socket with text commands for remaining scripts | proposed | 2026-11-04 | Keeps scripts like `empty` working without `hyprctl`. |
| 14 | Live upgrade by re-exec with state + fd handover | accepted | 2026-11-04 | Deploy new builds without losing open windows; possible only because all state is ours. |
| 15 | Single repo, Cargo workspace with one crate per building block, one binary | proposed | 2026-11-04 | Everything built together, but parallel and incremental compilation stays fast as the project grows. |
| 16 | Own screenshot taker without annotations | accepted | 2026-11-04 | Much simpler than an editor; replaces grim, slurp and swappy. |
| 17 | No system tray (StatusNotifier host) | accepted | 2026-11-04 | nm-applet's icon is dropped; network status is already in the bar. |
| 18 | Clipboard history persisted across reboots | accepted | 2026-11-04 | Same behaviour as cliphist today. |
| 19 | Own Wayland wire protocol implementation instead of libwayland/wayland-rs | accepted | 2026-11-04 | Live upgrade needs full ownership of client object maps and ids; libraries hide them. Tables are generated from the XML. |
| 20 | Hand-written FFI to C libraries, `libc` as the only crate | accepted | 2026-11-04 | Few functions are needed; no binding crates to track; fits the from-scratch rule. Supersedes the crate list in sec. 4. |
| 21 | Nested backend renders through GBM and hands dmabufs to the host | accepted | 2026-11-04 | Same render path as the future DRM backend; no libwayland-client or wl_egl_window needed. |
| 22 | Client buffers are uploaded at render time, then released | accepted | 2026-11-04 | Protocol handling never touches GL; only the render step needs the renderer. |
| 23 | One wallpaper, compiled into the binary, decoded at startup on a worker thread | accepted | 2026-11-04 | The owner only ever uses `/wallpapers/1.jpg`. `build.rs` embeds the file named in `config::WALLPAPER` (not committed to the repo); own baseline JPEG decoder, halved while it still covers 2560x1600, drawn with mipmaps and cropped to cover each output. Slideshow dropped. |
| 24 | Activation requests mark windows urgent instead of focusing them | accepted | 2026-11-04 | Same as Hyprland's default; urgency feeds the bar's red workspaces (M2). |
| 25 | KMS through raw kernel ioctls, no libdrm | accepted | 2026-11-04 | A dozen ioctls with stable uapi structs; same reasoning as ADR-020. |
| 26 | One output at a time until M4 | superseded by 33 | 2026-11-04 | All workspaces live on eDP-1 today; real multi-monitor rules need the `dawn`/monitor-manager inventory planned for M4. Order: `config::OUTPUT_PRIORITY`, then any connected output. |
| 27 | GPU memory on the render node, KMS on the card node | accepted | 2026-11-04 | The card fd survives a live upgrade, so everything allocated through it would leak; render-node allocations die with the process. Scanout buffers are imported into KMS via PRIME and freed by the next process after its first flip. |
| 28 | Upgrade blob with a format number and a copy of the previous binary | accepted | 2026-11-04 | `BLOB_FORMAT` changes on incompatible layout changes; a binary that cannot read the blob execs `$XDG_RUNTIME_DIR/davide-wm-prev` (copied at every start) with it, so the session survives. Textures travel as pixels, dmabufs are imported again, shm pools mapped again; keys held during the upgrade are forgotten. |
| 29 | The bar runs on its own thread, not in the main loop | accepted | 2026-11-04 | topbar's sources block (helpers up to 1 s, HTTP up to 3 s) and may panic; a thread keeps the compositor responsive and alive, and the port stays close to topbar. Its state is not part of the live upgrade: the new process starts a fresh bar. |
| 30 | `fontdue` and `ureq` (rustls) allowed in `crates/bar` | accepted | 2026-11-04 | Inherited from topbar: font rasterising and the Spotify Web API. Pure Rust; to be revisited (own TLS is out of scope). |
| 31 | Focus follows the mouse | accepted | 2026-11-04 | Like Hyprland's default (`follow_mouse = 1`): the toplevel under the pointer gets the keyboard focus, and a new window splits the window under the pointer. Workspace switches restore the last focused window until the pointer moves. |
| 32 | Launcher on the main thread, opened by tapping the main modifier | accepted | 2026-11-04 | Unlike the bar it has no blocking sources; keys go straight to it with no thread hop. The tap is detected in the key handler: the modifier pressed and released with no other key in between. |
| 33 | One primary output plus mirrors | accepted | 2026-11-04 | The owner works on one screen at a time (the MSI, or the laptop). Workspaces live on the primary output; any other lit output gets a scaled, letterboxed copy (`glBlitFramebuffer`). An unknown external monitor therefore mirrors the panel instead of extending it above (monitor-manager's "dual" layout), which also suits projectors; real extended layouts can come later. Policy in `backend/outputs.rs`, pure and tested; matching on the EDID model, never on the connector, which changes with the cabling. |
| 34 | Blanking inside the compositor | accepted | 2026-11-04 | No overlay window or `hyprctl`: blanking powers down every output except the `OUTPUT_BLANK_BLACK` ones (the OLED MSI, which would drop the signal and re-handshake), which get one black frame. Input during the 700 ms grace is ignored; then the first press, motion or scroll wakes and is swallowed, releases always pass so no key stays down in a client. Idle blanking skips while a client with a window on screen holds an idle inhibitor (monitor-manager ignored them). Screensaver settings persist in `$XDG_STATE_HOME/davide-wm/screensaver`. |
| 35 | Lid inhibitor as a logind fd held by the compositor | accepted | 2026-11-04 | Replaces the `systemd-inhibit sleep infinity` process and its pidfile; the fd survives a live upgrade in the blob. While held, closing the lid leaves the session running and only darkens the panel. |
| 36 | DP link check through `journalctl -k` | accepted | 2026-11-04 | After lighting an exclusive monitor, a worker job waits `LINK_CHECK_DELAY_MS` and greps the kernel log for amdgpu's `enabling link N failed`: when training fails, KMS still reports the output lit while it shows nothing. `/dev/kmsg` is not readable (`dmesg_restrict`). On failure the connector is skipped until unplugged and the panel comes back. A modeset error on an external monitor is handled the same way. |
| 37 | Clipboard history captured by the compositor itself | accepted | 2026-11-04 | No `wl-paste --watch`: on each new clipboard selection the compositor asks the owner for one type through a pipe and reads it on the worker (3 s deadline). Primary selection is not recorded, like cliphist. When the owner exits, the compositor keeps serving the captured clip. Picker and history share the launcher box (`Launcher::picker`). The history lives in the live-upgrade blob and in its own file, written on the worker. |
| 38 | Notification server on the main thread | accepted | 2026-11-04 | Uses `crates/dbus` (server side added) with the bus fd in the main epoll: calls are short and need compositor state. The name is requested with replace-existing and do-not-queue: when another server holds it (nested inside Hyprland) notifications stay off. Popups are cards drawn by `launcher::card` and composited like the launcher; the list reuses the picker. NotificationClosed is sent when a popup expires, while the entry stays in the list. Before switch-over, remove `~/.local/share/dbus-1/services/org.freedesktop.Notifications.service` and the `nwg-notifications` user service. |

<details>
<summary>ADR-001: Write the compositor from scratch</summary>

- **Context:** the goal is one optimised process that the owner fully understands; topbar and super already use raw protocol bindings instead of toolkits.
- **Decision:** no compositor framework. Allowed: thin bindings and protocol code (`wayland-server`, `drm`, `gbm`, `input`, `xkbcommon`, EGL/GL loaders, `x11rb`, `pipewire`).
- **Alternatives considered:** smithay (recommended by the assistant: ready DRM/libinput/renderers/protocols, used by niri and COSMIC); wlroots via FFI (mature C, unsafe-heavy bindings).
- **Consequences:** many months of work before parity; every protocol, the DRM atomic path, dmabuf handling and the XWM are ours to write and debug. smithay and niri remain the reference for how things are done.
</details>

<details>
<summary>ADR-014: Live upgrade by re-exec</summary>

- **Context:** once davide-wm is the daily session, every new build would otherwise mean logging out and losing every open window.
- **Decision:** the running process serialises its state into a versioned `memfd` blob, keeps the needed fds open across `execve`, and execs the new binary in the same PID, which restores the state and continues (sec. 6, "Live upgrade").
- **Alternatives considered:** client reconnection protocols (only some Qt/KDE clients support them; Chromium and foot do not); a tiny stable proxy process holding client connections (doubles every message, the proxy itself can never be upgraded).
- **Consequences:** every module must keep its state serialisable from M0 on (sec. 8); the blob format needs versioning and migrations; a failed restore falls back to the previous binary. Upgrade paths must be tested on the headless backend with real clients.
</details>

<details>
<summary>ADR-009: Own portal backend for screen sharing</summary>

- **Context:** browsers only screen-share through the xdg-desktop-portal ScreenCast API, which delivers video through PipeWire.
- **Decision:** davide-wm implements `org.freedesktop.impl.portal.ScreenCast` (and `Screenshot`) itself and produces the PipeWire stream with libpipewire.
- **Alternatives considered:** `wlr-screencopy` + xdg-desktop-portal-wlr (one more daemon, no window sharing); writing the PipeWire native protocol in-house (complex POD/negotiation, deferred).
- **Consequences:** xdg-desktop-portal and PipeWire stay as external requirements; davide-wm must install a `.portal` file and a `portals.conf` that routes ScreenCast/Screenshot to it.
</details>

---

## 10. Quality requirements
*Detail/expansion of sec. 1.2 with measurable quality scenarios.*

| Quality | Scenario | Metric / threshold |
|---------|----------|--------------------|
| Performance | Idle desktop, bar ticking every second | CPU and RSS below the measured baseline of Hyprland + topbar + super + helpers (baseline to be measured before M1) |
| Performance | Typing in foot on the MSI at 120 Hz | No missed vblank; key-to-frame ≤ 1 refresh |
| Reliability | A bar widget, the launcher or the notification server panics | Feature disabled and logged; compositor and clients keep running |
| Reliability | Any external call hangs (D-Bus peer, helper, HTTP) | Main loop iteration never exceeds its budget; timeout rule applied |
| Reliability | Compositor exits or crashes | VT and DRM master released; the TTY is usable again |
| Compatibility | Daily apps: foot, Chromium, VS Code, Thunar, Spotify, Audacity (X11), LibreOffice | Open, tile, fullscreen, clipboard and screen sharing in Chromium work |

---

## 11. Risks and technical debt
*Be honest: what is fragile, what was deferred, what needs watching.*

| Risk / debt | Impact | Mitigation / plan |
|-------------|--------|-------------------|
| From-scratch scope (DRM, dmabuf, XWM, portal, PipeWire) is very large | Years-long project; long time before switch-over | Strict milestones, each usable; Hyprland stays as fallback; read smithay/niri/wlroots as references. |
| Compositor crash = whole session lost | Lost work in every open app | Non-blocking rule, panic isolation, headless integration tests, nested development. |
| GPU/driver edge cases (dmabuf modifiers, atomic commit failures, hotplug) | Black screen, frozen outputs | Test hotplug with MSI and TV early (M1); keep a fallback modeset path. |
| libpipewire FFI | Unsafe code, one more C dependency | Isolated in `crates/screencast/`; reconsider a native implementation later. |
| Ported code inherits topbar debt: hand-written JSON/text parsers, compiled-in Spotify credentials, hard-coded machine paths (`BAT0`, `amdgpu_bl1`, `~/7Scripts`), a second D-Bus client in `crates/bar/src/dbus.rs` | Fragility, secrets in the binary, duplicated code | The bar's D-Bus client lost its monitor and notification code in M5 but still serves UPower, BlueZ and MPRIS; port those widgets to `crates/dbus`; move secrets out of the binary. |
| Some current behaviour depends on Hyprland specifics (socket2 consumers, `hyprctl` calls in helper scripts) | Regressions after switch-over | `dawn` inventoried in M4 (monitor-manager, blk, lid tools ported); still to check: the helper scripts the bar calls and `cast` (M7). |
| Live upgrade restores a wrong or partial state | Clients misbehave or disconnect after an upgrade | Versioned blob, `persist_struct!` forces every field to be handled, automatic fallback to `davide-wm-prev`. Still missing: round-trip tests on a headless backend, and a test of the DRM path (logind bus, KMS fd, libinput re-open). |
| One huge project, always built together | Long builds, harder refactors | Cargo workspace (ADR-015), clear crate boundaries, live upgrade makes deploying cheap. |
| Remaining simplifications | Visual glitches with some clients | Subsurfaces are always desynchronised, popups have no constraint adjustment or grabs, full redraw every frame, buffer scale 1 only, no drag and drop, fullscreen requests before the first commit are ignored. |
| Open questions | Plan may change | Whether to keep layer-shell for any external client. |

---

## 12. Glossary
*Domain and technical terms specific to the project. Removes ambiguity for everyone (the AI included).*

| Term | Definition |
|------|------------|
| Compositor | The Wayland display server: owns outputs and input, composites client buffers, implements protocols. davide-wm is compositor and window manager in one. |
| DRM/KMS | Linux kernel display API: modesetting (KMS) and buffer scanout; atomic commits change a whole output state at once. |
| GBM | Mesa's buffer allocator for scanout-capable buffers. |
| dmabuf | Kernel handle to a GPU buffer, shared between clients and the compositor without copies. |
| Direct scanout | Showing a client buffer on a display plane without compositing it. |
| Dwindle | Binary-split tiling: each new window splits the focused area in two, alternating direction. |
| XWM | X11 window manager run by the compositor to manage windows created through Xwayland. |
| Portal backend | Implementation of `org.freedesktop.impl.portal.*` that the xdg-desktop-portal frontend forwards requests to. |
| blk | Black-screen feature from `dawn`: blanks outputs instead of locking. |
| dawn | `~/0Projects/dawn`, the owner's collection of desktop helper crates (monitor-manager, blk, lid-action, cast, ...). |
| Tap-Super | Pressing and releasing Super with no other key in between; opens the launcher. |
| Nested backend | Running davide-wm as a window inside another Wayland compositor for development. |

---

*Template based on [arc42](https://arc42.org) — adapted to Markdown. Original: `lancini.net/README`.*
