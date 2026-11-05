## UI/UX proposal for rynas: a web interface for managing a professional NAS

Lessons from earlier homemade DSM packages (web interface: cache, first paint and theme) apply here too, in particular the rules on first paint and server-saved theme (§2.8). I computed the WCAG contrast ratios myself with a script, using the WCAG 2.2 formula.

---

### 0. How the reference interfaces are built

| Product | Approach | Pros | Cons for a professional NAS |
|---|---|---|---|
| **Synology DSM 7** | Desktop with multiple windows, main menu, desktop widgets. Storage Manager with drawn bays | You can do more than one thing in parallel, it feels like an operating system, the physical disk view is excellent | Windows have no URL of their own (no deep links or history), does not work on mobile (there is a separate app), heavy, not dense |
| **TrueNAS SCALE 25.x** | Fixed sidebar and configurable widget dashboard (Configure mode, drag and drop, groups with predefined layouts) | Readable, every page has its own URL, customizable dashboard | The widget layout system is rigid, pages are long, settings are scattered |
| **OpenMediaVault 7** | Angular Material workbench with tree sidebar. Yellow "pending changes" bar with Apply (`dirtymodules.json`) | Configuration is prepared first and then applied, explicitly. Very dense | Dated graphics. The bar does not say which modules change |
| **Unraid 7.2** | Top tabs and tile dashboard. Responsive only since 7.2 (1 column below 768px, 2 columns between 768 and 1599px) | Tiles can be dragged, rich plugin ecosystem | Space goes unused on wide screens (reported as a bug), plugins break when the markup changes |
| **Proxmox VE** | Resource tree on the left, tabbed panel, task log at the bottom | Scales to many objects, task log always visible, high density | Aging ExtJS, steep learning curve, poor on mobile |
| **Cockpit / PatternFly** | Sidebar with one page per service, PatternFly components, built-in terminal | Mature and accessible design system, page-based plugins | Poor dashboard, no overview |
| **HexOS / ZimaOS** | Simplified "Command Deck", apps and status at the center | Simple, designed for mobile | Too simplified for advanced use (HexOS depended on the cloud in beta) |
| **Grafana, Vercel (Geist), Linear, Tailscale, Cloudflare, Portainer, Dockhand** | Compact sidebar, header with actions, dense tables, Ctrl+K, toasts, polished empty states, environment/host selector | Fast to use with the keyboard, consistent, URLs describe state | Nothing "physical" (disks, bays) |

**Summary.** What works best today is a sidebar with URLs, a widget dashboard and a command palette. From DSM it is worth taking only the physical storage view. From OMV the "pending changes" flow. From Proxmox the task log and object-based management.

---

### 1. Three alternative layouts

#### A. Sidebar and widget dashboard (recommended)
```
┌──────────────────────────────────────────────────────────────────────┐
│ ◆ rynas  nas01 ▾     [ Search or command… Ctrl+K ]   ⟳2  🔔3  ☾  admin▾│
├────────────┬─────────────────────────────────────────────────────────┤
│ ▣ Dashboard│ Dashboard                                 [Edit layout] │
│ ─ Storage  │ ┌─────────────┐┌─────────────┐┌──────────────────────┐  │
│   Pools    │ │ CPU  23%    ││ RAM 41%     ││ Net   ▁▃▅▇▅▃ 940Mb/s │  │
│   Disks    │ │ ▁▂▃▂▅▃▂     ││ 13/32 GB    ││ eth0 ↑12 ↓880        │  │
│   Snapshots│ └─────────────┘└─────────────┘└──────────────────────┘  │
│ ─ Shares   │ ┌──────────────────────────────┐┌─────────────────────┐ │
│   SMB/NFS  │ │ Pool "tank"  RAIDZ2 ● Healthy││ Running tasks       │ │
│ ─ Users    │ │ ███████████░░░░ 68% 14/20TB  ││ ▸ Scrub tank  42%   │ │
│ ─ Services │ │ [■][■][■][■][■][□] bays      ││ ▸ Replication ✓     │ │
│ ─ Plugins  │ └──────────────────────────────┘└─────────────────────┘ │
│   ⊕ Docker │ ┌─────────────────────────────────────────────────────┐ │
│   ⊕ Backup │ │ Alerts: ⚠ sdc temperature 52°C   ⓘ update 0.4.2     │ │
│ ─ System   │ └─────────────────────────────────────────────────────┘ │
├────────────┴─────────────────────────────────────────────────────────┤
│ ⟳ 2 pending changes: SMB, Avahi                  [Review] [Apply]   │
└──────────────────────────────────────────────────────────────────────┘
```
- **Pros:** every view has its own URL (deep links, back button, pages openable in multiple browser tabs). It is the best-known model and adapts well to mobile and to plugins (menu entry plus widget).
- **Cons:** less original, and a multi-step job requires switching pages. Ctrl+K and the detail side sheet (side panel) make up for it.

#### B. Windowed desktop, DSM style
```
┌──────────────────────────────────────────────────────────────────────┐
│ ☰  ▣Storage  ▣Docker  ▣Log                       🔔  ⟳  12:04  admin │
├──────────────────────────────────────────────────────────────────────┤
│  [Storage]  [Shares]  ┌─ Storage Manager ─────────────────── _ □ × ┐ │
│  [Users]    [Docker]  │ Pools │ ┌──┐┌──┐┌──┐┌──┐  tank RAIDZ2 ●     │ │
│                       │ Disks │ │1 ││2 ││3 ││4 │  68% used          │ │
│  ┌ CPU widget ┐       │ Cache │ └──┘└──┘└──┘└──┘                    │ │
│  │ ▁▃▅▃ 23%   │       └───────┴─────────────────────────────────────┘ │
│  └────────────┘           ┌─ Log ─────────────────────── _ □ × ┐      │
│                           │ 12:03 smbd reload ok                │     │
└───────────────────────────┴─────────────────────────────────────┴─────┘
```
- **Pros:** several things can be worked on in parallel and Synology users feel at home immediately.
- **Cons:** a window manager has to be written (z-index, focus, resizing). Accessibility is hard (focus traps, screen readers), no URL per window, unusable on mobile, larger WASM bundle. On a professional NAS windows are of little use: you do one operation at a time.

#### C. Resource tree and command palette, Proxmox style
```
┌──────────────────────────────────────────────────────────────────────┐
│ ◆ rynas   [ > snapshot tank/media …                       Ctrl+K ]   │
├──────────────────┬───────────────────────────────────────────────────┤
│ ▾ nas01          │ tank/media  (dataset)                             │
│   ▾ ◉ tank       │ [Summary][Snapshots][Permissions][Quotas][Repl.]  │
│     ▸ media      │ ┌───────────────────────────────────────────────┐ │
│     ▸ backup     │ │ Used 4.2 TB  Compression lz4 1.31x            │ │
│   ▸ ◉ fast (SSD) │ │ Shares: \\nas01\media (SMB), /media (NFS)      │ │
│   ▸ ⇄ Shares     │ └───────────────────────────────────────────────┘ │
│   ▸ ▣ Containers │                                                   │
├──────────────────┴───────────────────────────────────────────────────┤
│ Tasks: ✓ 12:01 snapshot tank/media@auto  ⟳ 12:03 scrub tank 42%  ▴   │
└──────────────────────────────────────────────────────────────────────┘
```
- **Pros:** very dense, object-based navigation (pool → dataset → share), task log always visible, experts like it.
- **Cons:** intimidates less experienced users, the tree has no dashboard and it is not suited to mobile.

**Recommendation:** take A as the structure and add two things from C:
- a **collapsible task panel at the bottom**, Proxmox style;
- an **optional tree** in the Storage section (pool → dataset → snapshot).

B's desktop is ruled out. Of its style only the physical bay view remains.

---

### 2. Design system

#### 2.1 Semantic tokens (CSS custom properties, `data-theme`)
| Token | Light | Dark | Notes |
|---|---|---|---|
| `--bg` | #e6e7ed | #1a1b26 | app background |
| `--bg-sunken` | #d5d6db | #16161e | sunken areas, code, logs |
| `--surface` | #ffffff | #24283b | cards, dialogs |
| `--surface-nav` (drawer) | #fafafa | #12121c | sidebar |
| `--surface-appbar` | #e6e7ed | #1f2335 | top bar |
| `--surface-hover` | #e9ebf2* | #434b60 (TableHover) | |
| `--surface-striped` | #f4f5f8* | #16161d | alternating rows |
| `--border` | #d5d6db | #292e42 (Divider) | decorative dividers |
| `--border-strong` | **#7c7f8c*** | **#6b7399*** | inputs and controls (≥3:1, see §5) |
| `--text` | #343b58 | #c0caf5 | |
| `--text-muted` | #40434f | #a9b1d6 | |
| `--text-subtle` | **#5f6273*** | **#868fb8*** | placeholders, captions |
| `--text-disabled` | #6c6e75 | #565f89 | disabled elements only |
| `--accent` | #2959aa | #7aa2f7 | links, primary button, focus |
| `--accent-2` | #5a3e8e | #bb9af7 | plugins, tags, series 2 |
| `--info` | #0f4b6e | #7dcfff (#2ac3de tertiary) | |
| `--success` (text) | **#1f6f61*** | #73daca | |
| `--success-fill` | #3a8f7f | #73daca | badges, bars |
| `--warning` (text) | **#8a5a00*** | #e0af68 | |
| `--warning-fill` | #df8e1d (text #1a1b26) | #e0af68 | |
| `--danger` | #8c4351 | #f7768e | |
| `--on-accent` | #ffffff | #1a1b26 | text on filled buttons |

Values marked with an asterisk are derived to fix contrast. All others are the original Fusion palette (the author's own colour scheme). For badge background tints I suggest `color-mix(in oklch, var(--success) 14%, var(--surface))`, so no colors need to be added to the palette.

#### 2.2 Fonts (all OFL-licensed)
- **Inter** (variable, with `font-feature-settings: "tnum","cv11"` for tabular figures), [rsms.me/inter](https://rsms.me/inter/). Alternatively IBM Plex Sans, [ibm.com/plex](https://www.ibm.com/plex/), more "technical".
- **JetBrains Mono** for paths, disk IDs, logs and terminal, [jetbrains.com/lp/mono](https://www.jetbrains.com/lp/mono/).
- Fonts must be bundled locally (self-hosted, the NAS may be offline), as woff2 subset to the characters in use.

#### 2.3 Type scale (ratio ~1.2, 14px base, console-like density)
12 (captions, badges) · 13 (compact tables) · **14 (body)** · 16 (card titles) · 20 (page H2) · 24 (H1) · 32 (dashboard KPIs). Weights 400/500/600, line height 1.45 for body and 1.2 for headings.

#### 2.4 Spacing, radius, density
- 4px grid: 4, 8, 12, 16, 24, 32, 48.
- Radius: 4px for inputs and badges, 8px for cards and dialogs, 12px for large modals, pill for statuses.
- Two densities, chosen per user:

| | Comfortable | Compact |
|---|---|---|
| Table rows | 40px | 32px |
| Inputs | 36px | 28px |
| Card padding | 16px | 12px |

  Implemented as `[data-density=compact]` overriding the `--space-*` and `--row-h` tokens.
- Minimal shadows: in the dark theme surfaces are separated by lightness and a 1px border, as in Linear and Vercel ([vercel.com/geist](https://vercel.com/geist)).

#### 2.5 Icons
- **Lucide** ([lucide.dev](https://lucide.dev), ISC): consistent, 1.5–2px stroke, has icons like `hard-drive`, `server`, `database` and `shield`.
- For missing specific icons (RAID, SAS, UPS), **Tabler** ([tabler.io/icons](https://tabler.io/icons), MIT, over 5000 icons) with the same stroke. Phosphor ([phosphoricons.com](https://phosphoricons.com)) is fine but has a different style: better not to mix it with the other two.
- Icons are inserted as an SVG sprite with `currentColor`, so they inherit the theme.

#### 2.6 Charts
- **uPlot** ([github.com/leeoniya/uPlot](https://github.com/leeoniya/uPlot)): about 50 KB, suited to real-time time series (CPU, I/O, network), easy to bind from WASM. For occasional pie charts and treemaps, **ECharts**, loaded only when needed (lazy).
- Series colors: accent → accent-2 → info → success → warning, always combined with shape and pattern (see §5).

#### 2.7 States
Every state always has icon, label and color together, never color alone:
- ● Healthy
- ▲ Degraded
- ✕ Failed
- ◌ Unknown

#### 2.8 Theme
The theme is saved on the server per user, and the browser keeps a copy for the first paint. An inline script in `<head>` sets `data-theme` and the background before the CSS loads: this is the lesson from earlier DSM packages, and it avoids the white flash (it happens with Brave, which clears localStorage).

---

### 3. Key patterns

1. **Dashboard with reorderable widgets.** 12-column CSS grid with S/M/L/XL sizes. An explicit "Edit" mode (like TrueNAS) enables drag and drop plus a keyboard alternative (arrows plus space). The layout is saved per user with per-role presets. Plugin widgets can be added, with the constraint of not leaving empty columns (the Unraid 7.2 flaw).

2. **Visual Storage Manager.** "Chassis" view with bays drawn from the hardware profile (profile file, or SES enclosure / `sg_ses`, or a generic grid). Each bay shows:
   - a status LED with distinct color and shape;
   - model, size and temperature;
   - the pool it belongs to, with a per-pool colored border.

   Clicking a bay opens a side sheet with SMART, history and an "Identify" button that blinks the LED. Next to it is the logical view: pool → vdev → disks, with the tree. For Synology users it is DSM's most appreciated feature.

3. **Pool and share creation wizard.** A stepper with 4–5 steps:
   1. disk selection on the bay view;
   2. layout (mirror, RAIDZ1/2/3) with real-time calculation of usable capacity and fault tolerance;
   3. options (encryption, compression, ashift);
   4. summary with the **command preview** (`zpool create …`) and the phrase to type to confirm destructive actions.

4. **Pending changes.** Inspired by OMV, but fixing its limit: the bottom bar **lists the affected services** and shows a **diff** before applying, with "Apply", "Discard" and "Apply and verify" buttons (automatic rollback if the session is lost, as on routers). Immediate and pending changes must be distinguished consistently throughout the interface.

5. **Notifications and task center.**
   - Toasts for short outcomes.
   - Bell with persistent notifications, filterable by severity, with "mark as read" and a link to the resource.
   - Bottom task panel, Proxmox style: progress, streaming log, cancel. Long tasks (scrub, resilver) keep running when you change page.

6. **Command palette (Ctrl+K)** and global search, unified:
   - navigation ("go to Disks");
   - actions ("create snapshot of tank/media");
   - entities (users, shares, containers, disks by serial);
   - commands registered by plugins.

   Client-side fuzzy search over the entity index (in Rust, compiled to WASM), with grouping, shortcuts shown alongside and prefixes (`>` for actions, `@` for users). The model is Linear / Vercel.

7. **Share permissions as a matrix.** Rows = users and groups, columns = shares. Each cell R/RW/Deny/–, distinguishing inherited from explicit permissions (marked with a dot). Bulk editing by selecting multiple cells. The tooltip explains the effective permission ("RW from group admin"). There is an "effective permission for user X" view and a preview of the resulting POSIX/NFSv4 ACLs.

8. **Log viewer.** Virtualized table with:
   - monospace font;
   - chip filters (unit, severity, time range);
   - live mode (tail) with automatic pause on scroll;
   - highlighting of search terms;
   - shareable links with filters in the URL;
   - export.

   Sources are journald, service logs and the audit log.

9. **Web terminal.** [xterm.js](https://xtermjs.org) over WebSocket and PTY. Its theme is derived from the tokens (the Tokyo Night ANSI palette matches) and it opens in a page, in a bottom panel or in a tab. It requires re-authentication (sudo) and every session goes to the audit log.

10. **Responsive and mobile:**
    - below 1024px the sidebar becomes a drawer;
    - below 768px there is a bottom bar with 4 entries (Dashboard, Storage, Notifications, Menu) and widgets in one column;
    - tables become card lists with actions in the "⋯" menu;
    - the wizard goes full screen;
    - the terminal is discouraged on mobile, but remains available.

---

### 4. How a plugin enters the UI

The model is VS Code's "contribution points": the plugin does not add free-form HTML, it **declares in its manifest** what it adds and uses rynas components and tokens.

```toml
[ui]
nav       = { section = "Plugins", label = "Docker", icon = "container", route = "/p/docker" }
widgets   = [{ id = "docker.summary", sizes = ["S","M"], title = "Containers" }]
pages     = [{ route = "/p/docker/*" }]
panels    = [{ target = "storage.dataset.tabs", label = "Docker volumes" }]
settings  = { schema = "settings.schema.json" }   # automatically generated form
commands  = [{ id = "docker.restart", label = "Restart container…" }]
notifications = ["docker.update-available"]
```

- **Menu entry:** added only under "Plugins" or in an existing section, if the manifest declares it. Uses a Lucide icon and shows a plugin status badge.
- **Widgets:** enter the dashboard catalog with the declared sizes, the same loading and error states and the same frame as system widgets.
- **Page:** route `/p/<plugin>/…` inside the rynas layout (breadcrumb and header with actions are provided by rynas).
- **Panels in existing pages:** they hook into named slots (`storage.dataset.tabs`, `share.detail.sidebar`, `user.detail.sections`). The host decides the order and the plugin cannot reorder other panels.
- **Settings:** described with a JSON Schema, and rynas generates the form, consistent with the rest and accessible. They appear under System → Plugins → <name> and take part in pending changes.
- **Technique:** a Rust/WASM plugin compiled as a component with a declared API (UI kit exposed by the host), or an isolated micro-frontend in a sandboxed iframe with CSS tokens passed via `postMessage`. The first gives consistency, the second isolation: I recommend the first for official plugins and the second for third-party ones.
- **Review rules** (a manifest linter): no hard-coded colors, tokens only; icons from the official set; translatable strings (i18n).

---

### 5. Accessibility: WCAG 2.2 contrast

Thresholds are 4.5:1 for normal text, 3:1 for large text and for UI components (criterion 1.4.11), [w3.org/TR/WCAG22](https://www.w3.org/TR/WCAG22/).

**Compliant pairs:**
- primary and secondary text on all backgrounds: light 6.8 to 11.0, dark 6.9 to 11.5;
- light Primary, Secondary, Tertiary and Error on white: 6.8 to 9.3;
- all dark accents on Background and Surface: 5.5 to 10.3.

**Non-compliant pairs and proposed fixes:**

| Pair | Contrast | Problem | Fix |
|---|---|---|---|
| Warning #df8e1d on white | **2.62** | unreadable text and icons | warning text **#8a5a00** (5.93 on white, 4.80 on #e6e7ed). For filled badges: #df8e1d background with **#1a1b26** text (6.53) |
| Warning #df8e1d on #e6e7ed | 2.12 | as above | as above |
| White text on Warning #df8e1d | 2.62 | buttons and badges | dark text #1a1b26 |
| Success #3a8f7f on white / Background | 3.88 / 3.14 | only fine as a fill color | success text **#1f6f61** (5.99 / 4.86) |
| White text on Success #3a8f7f | 3.88 | button text | background #2f7a6c (5.10) or text #1a1b26 (4.41: not enough for normal text, use only large or bold) |
| Light TextDisabled #6c6e75 on Background | 4.13 | only fine for truly disabled elements (exempt) | for placeholders and captions **#5f6273** (4.89) |
| Each light accent on BackgroundGray #d5d6db | Success 2.67, Warning 1.80, Primary 4.66 | | do not put colored text on sunken backgrounds: use them for logs and code with neutral text |
| Dark TextDisabled #565f89 on Bg / Surface | **2.76 / 2.35** | Tokyo Night uses it for comments: unreadable placeholders and captions | `--text-subtle` **#868fb8** (5.40 / 4.60) and #565f89 only for disabled elements |
| Dark LinesInputs #414868 as input border | **1.91** on Bg, 1.63 on Surface | fails criterion 1.4.11 | `--border-strong` **#6b7399** (3.69 / 3.15). #414868 stays for decorative dividers |
| Dark Divider #292e42 | 1.27 | | fine only as decoration, never as the only boundary of a control |
| Dark TextSecondary on TableHover #434b60 | **4.12** | hovered rows | on hover use TextPrimary (5.39), or lighten the hover to #2f3549 |
| Dark accents on TableHover (Primary 3.45, Error 3.29) | | links and states in hovered rows | darker hover or primary text |
| Light input borders (#d5d6db on white) | ~1.4 | fails criterion 1.4.11 | **#7c7f8c** (3.98 on white, 3.23 on #e6e7ed) |

**Other measures:**
- **Focus:** 2px ring in `--accent` with 2px offset (Primary on Background: light 5.48, dark 5.78 on Surface), always visible with `:focus-visible`.
- **States:** never color alone. Always icon and text, striped pattern for "degraded" in the bay view, different shapes for chart series (useful for color-blind users: dark Success and Warning have similar luminance).
- **Keyboard:** every function is reachable from the keyboard, including dashboard drag and drop. Dialogs with `aria-modal` and focus trapped inside. Live notifications in `aria-live="polite"`, critical errors in `assertive`.
- **Animations:** respect `prefers-reduced-motion`.
- **Targets:** minimum 24×24px (criterion 2.5.8), 32px in compact density.

---

**Sources:**
- TrueNAS: [Dashboard 25.10](https://www.truenas.com/docs/scale/25.10/scaleuireference/scaledashboard/) · [Customization](https://www.truenas.com/docs/scale/dashboard/customizingdashboard/)
- Unraid: [7.2.0 release notes](https://docs.unraid.net/unraid-os/release-notes/7.2.0/) · [Responsive plugin migration guide](https://forums.unraid.net/topic/192172-responsive-webgui-plugin-migration-guide/) · [Two-column bug](https://forums.unraid.net/bug-reports/stable-releases/720-dashboard-does-not-utilize-full-available-screen-width-and-is-restricted-to-two-columns-r4137/)
- OpenMediaVault: [How it works / dirtymodules](https://docs.openmediavault.org/en/7.x/development/howitworks.html) · [Workbench](https://docs.openmediavault.org/en/7.x/administration/general/workbench.html)
- HexOS and ZimaOS: [HexOS beta review](https://nascompares.com/2024/12/02/hexos-nas-software-review-of-the-beta/) · [ZimaOS 1.5](https://shop.zimaspace.com/pages/zimaos-1-5-changes) · [ZimaOS vs DSM](https://shop.zimaspace.com/blogs/zima-campaign-hub/zimaos-dsm-nas-experience)
- Dockhand: [README](https://cdn.jsdelivr.net/gh/finsys/dockhand@main/README.md)
- Other references: [Proxmox VE GUI](https://pve.proxmox.com/pve-docs/chapter-pve-gui.html) · [Cockpit](https://cockpit-project.org/) · [PatternFly](https://www.patternfly.org/) · [Synology DSM](https://www.synology.com/en-global/dsm) · [Grafana dashboards](https://grafana.com/docs/grafana/latest/dashboards/) · [Vercel Geist](https://vercel.com/geist)

For DSM, Proxmox, Cockpit, Linear, Tailscale and Cloudflare the interface descriptions come from prior knowledge, not from pages read in this session. The contrast ratios, on the other hand, were computed.
