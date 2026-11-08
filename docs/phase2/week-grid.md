# Weekly schedule grid (7 days × 24 hours)

A reusable UI component (`crates/rynas-ui/src/week_grid.rs`) for anything that takes a level
per hour of the week: LED brightness (normal / reduced / off), backup and replication run
windows, bandwidth limits by hour, notification quiet hours, fan quiet hours. The demo build
shows samples at `/dev/week-grid`.

Users:

- Hardware › Synology, **front LED brightness schedule** (`modules/synology/led.rs`, synohwd's
  `led_schedule`, synodeb ≥ 4.8; see `docs/plugins/synology.md`): levels `0` Normal (Full),
  `1` Reduced (Half), `2` Off (Off); the import of a schedule exported from the Synology system
  (its parser and messages) lives in that module, not in the component.

## 1. Research

| Where | What it does | Taken |
|---|---|---|
| Synology DSM, Control Panel › Hardware & Power, LED brightness ([KB][syn-led], [forum][nasforum]) | Brightness levels per hour of the week, set on a 7 × 24 grid; the schedule is stored as a string of 168 characters, Sunday first, one digit per hour | Value format (168 chars, Sunday first) so a schedule read from synohwd or migrated is stored unchanged; levels picked first, then painted |
| DSM package schedules (Cloud Sync "suspend" window ([KB][syn-cloudsync]), Download Station, Surveillance recording schedule) | Same grid: pick a mode, drag a rectangle; click a day or hour heading to fill it | Rectangle drag, header fill |
| TrueNAS cloud sync ([docs][truenas-cs], [API][truenas-api]) | Bandwidth schedule as a list of change points (`08:00,512k 18:00,off`, rclone `--bwlimit` format); no grid | A grid value converts to change points easily (`summary` runs); the grid is easier to read for a week |
| Genetec Security Center weekly schedules ([docs][genetec]) | Click and drag selects or clears a continuous block of a 7-day grid | Confirms the drag model |
| WAI-ARIA APG grid pattern ([APG][apg]) | `role=grid` / `row` / `gridcell`, one Tab stop with a roving `tabindex`, arrows move, Space/Enter act | Roving focus, arrows (visual direction in both orientations), Home/End, Space paints, Shift+Space fills the day, digits pick the level, each cell labelled "Monday 08:00–09:00: Reduced" |
| Pointer Events for touch drag ([javascript.info][jsinfo], [MDN-style write-up][devto]) | `touch-action: none` on the surface or the browser takes the gesture; `setPointerCapture` keeps the stream; with capture the target stays the first element, so the cell under the finger comes from `elementFromPoint` | Mouse and pen: exactly that; `pointercancel` puts the value back. Touch: see below |
| Touch drag-to-select inside a scrolling page (dnd-kit `TouchSensor` delay + tolerance activation, [use-gesture extras][usegesture], [non-passive listeners][passive]) | A surface with `touch-action: none` traps page scrolling on phones; the usual way out is to let the browser pan (`pan-x pan-y`) and start the drag only after a hold that did not move, then cancel `touchmove` from a non-passive listener | Tap paints one cell; a 350 ms hold (cell outline grows as the cue) that stays within 8 px starts the rectangle, then a non-passive `touchmove` on the grid blocks scrolling until the finger lifts; moving first is a scroll |

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
[usegesture]: https://use-gesture.netlify.com/docs/extras
[passive]: https://bobbyhadz.com/blog/unable-to-prevent-default-inside-passive-event-listener

## 2. Value

- 168 ASCII characters, **Sunday first**, `value[day * 24 + hour]` (day 0 = Sunday); the
  same layout as the Synology system's schedules, so such a schedule is stored unchanged. The
  component only checks (`validate`: length, only level codes) or repairs (`normalize`) it.
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

week_grid(levels: &'static [GridLevel], value: RwSignal<String>, label_key,
          on_commit: Option<Callback<String>>) -> AnyView   // on_commit: once per finished change
week_grid_preview(levels, value: Signal<String>, label_key) -> AnyView   // read-only, 24 × 7 mini
week_grid_legend(levels) -> AnyView
validate, normalize, filled, index, set_rect, summary, summary_text
```

2–4 levels. Colours are theme tokens (accent and its mixes with the surface, the strong border
for "off"), so light and dark follow the theme; the pattern tells levels apart without colour.

## 4. Layout and interaction

- Pick the level (segmented control, also the legend), then click or drag a rectangle; a day
  heading fills the day, an hour heading the hour on every day. The value updates while dragging;
  `on_commit` and the `aria-live` summary only at the end of the drag. Esc during a drag puts
  the value back; only the pointer that started the drag paints (a second finger is ignored).
- Touch: the page scrolls over the grid (`touch-action: pan-x pan-y`); tap = one cell, hold
  350 ms without moving = start of a rectangle, then the grid keeps the finger.
- Keyboard: arrows, Home/End, Space paints, Shift+Space fills the day, Shift+Enter the hour on
  every day, digits pick the level (not with Ctrl/Alt/Meta held).
- Each cell has a memo of its level, so a change re-renders only the cells whose level changed.
- No horizontal scroll at any width: the grid sits in a `wg` size container; below 560 px of
  container width days become columns and hours rows (24 rows of 20 px), so a phone gets cells
  about 40 px wide instead of 12 px. Arrow keys follow the visual direction.
- The CSS is one contiguous block after `/* week grid */` in `style/main.css`.
