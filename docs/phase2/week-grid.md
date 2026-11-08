# Weekly schedule grid (7 days × 24 hours)

A reusable UI component (`crates/rynas-ui/src/week_grid.rs`) for anything that takes a level
per hour of the week: LED brightness (normal / reduced / off, first user, backend from synohwd
later), backup and replication run windows, bandwidth limits by hour, notification quiet hours,
fan quiet hours. No feature uses it yet; the demo build shows it at `/dev/week-grid`.

## 1. Research

| Where | What it does | Taken |
|---|---|---|
| Synology DSM, Control Panel › Hardware & Power, LED brightness ([KB][syn-led], [forum][nasforum]) | Brightness levels per hour of the week, set on a 7 × 24 grid; the schedule is stored as a string of 168 characters, Sunday first, one digit per hour | Value format (168 chars, Sunday first) so a schedule read from synohwd or migrated is stored unchanged; levels picked first, then painted |
| DSM package schedules (Cloud Sync "suspend" window ([KB][syn-cloudsync]), Download Station, Surveillance recording schedule) | Same grid: pick a mode, drag a rectangle; click a day or hour heading to fill it | Rectangle drag, header fill |
| TrueNAS cloud sync ([docs][truenas-cs], [API][truenas-api]) | Bandwidth schedule as a list of change points (`08:00,512k 18:00,off`, rclone `--bwlimit` format); no grid | A grid value converts to change points easily (`summary` runs); the grid is easier to read for a week |
| Genetec Security Center weekly schedules ([docs][genetec]) | Click and drag selects or clears a continuous block of a 7-day grid | Confirms the drag model |
| WAI-ARIA APG grid pattern ([APG][apg]) | `role=grid` / `row` / `gridcell`, one Tab stop with a roving `tabindex`, arrows move, Space/Enter act | Roving focus, arrows (visual direction in both orientations), Home/End, Space paints, Shift+Space fills the day, digits pick the level, each cell labelled "Monday 08:00–09:00: Reduced" |
| Pointer Events for touch drag ([javascript.info][jsinfo], [MDN-style write-up][devto]) | `touch-action: none` on the surface or the browser takes the gesture; `setPointerCapture` keeps the stream; with capture the target stays the first element, so the cell under the finger comes from `elementFromPoint` | Exactly that; `pointercancel` puts the value back |

Unraid and openmediavault have no weekly level grid (cron fields or start/stop times per job).

[syn-led]: https://kb.synology.com/en-my/DSM/tutorial/Which_Synology_DiskStation_supports_LED_brightness_adjustment
[nasforum]: https://www.nas-forum.com/forum/topic/51287-gestionleds
[syn-cloudsync]: https://kb.synology.com/en-my/DSM/tutorial/Cloud_Sync_affects_Synology_NAS_hibernation
[truenas-cs]: https://www.truenas.com/docs/core/13.3/uireference/tasks/cloudsynctasks/
[truenas-api]: https://api.truenas.com/v25.10.0/api_methods_cloudsync.update.html
[genetec]: https://techdocs.genetec.com/r/en-US/Security-Center-Administrator-Guide-5.13/Defining-weekly-schedules
[apg]: https://www.w3.org/WAI/ARIA/apg/patterns/grid/
[jsinfo]: https://javascript.info/pointer-events
[devto]: https://dev.to/nishinoshake/smooth-drag-interactions-with-pointer-events-5e2j

## 2. Value

- 168 ASCII characters, **Sunday first**, `value[day * 24 + hour]` (day 0 = Sunday): the DSM
  format. `from_dsm` / `to_dsm` are the identity plus `validate` (length, only level codes).
- The week is **shown Monday first** for en and it (the app has no first-day setting; add one to
  `index` if a locale needs Sunday). `index(row, hour)` maps a display cell to the stored one;
  `rotate_for_display` / `rotate_to_storage` convert whole strings.
- The first level is the default: unreadable values show as that level (with a note) and it is
  left out of the summary ("Mon–Fri 17–24 Reduced; Sat–Sun 00–24 Off", "All week: Normal").
  Runs stop at midnight.

## 3. API

```rust
pub struct GridLevel { pub code: char, pub label_key: &'static str, pub tone: GridTone }
pub enum GridTone { Full, Half, Low, Off }   // solid, stripes, dots, outlined: not colour only

week_grid(levels: &'static [GridLevel], value: RwSignal<String>, label_key) -> AnyView
week_grid_preview(levels, value: Signal<String>, label_key) -> AnyView   // read-only, 24 × 7 mini
week_grid_legend(levels) -> AnyView
validate, normalize, filled, index, set_rect, summary, summary_text, from_dsm, to_dsm
```

2–4 levels. Colours are theme tokens (accent and its mixes with the surface, the strong border
for "off"), so light and dark follow the theme; the pattern tells levels apart without colour.

## 4. Layout and interaction

- Pick the level (segmented control, also the legend), then click or drag a rectangle; a day
  heading fills the day, an hour heading the hour on every day. The value updates while dragging.
- No horizontal scroll at any width: the grid sits in a `wg` size container; below 560 px of
  container width days become columns and hours rows (24 rows of 20 px), so a phone gets cells
  about 40 px wide instead of 12 px. Arrow keys follow the visual direction.
- The CSS is one contiguous block after `/* week grid */` in `style/main.css`.
