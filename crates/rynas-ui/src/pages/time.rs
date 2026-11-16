//! Sistema > Data e ora: time zone, automatic sync, manual time (`/api/v1/system/time`, D-TIME).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::clock::{ClockSettings, ClockState, MAX_SERVERS, MAX_YEAR, MIN_YEAR, SetClock, TimeDaemon, ZoneInfo};

use crate::form::{Draft, area_field, form_error, fid, spinner, stale_notice, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons;
use crate::toast::toast;
use crate::ui::{self, PageHeader, Remote, Tone};

const URL: &str = "/api/v1/system/time";

/// What the form edits; the servers as typed (one per line).
#[derive(Clone, PartialEq)]
struct Form {
    tz: String,
    sync: bool,
    servers: String,
}

fn form_of(s: &ClockState) -> Form {
    Form { tz: s.timezone.clone(), sync: s.sync_enabled, servers: s.servers.join("\n") }
}

fn servers(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

fn settings(f: &Form, local_rtc: bool) -> ClockSettings {
    ClockSettings { timezone: f.tz.clone(), sync_enabled: f.sync, servers: servers(&f.servers), local_rtc }
}

/// "UTC+01:00".
fn utc(off: i32) -> String {
    let a = off.unsigned_abs();
    format!("UTC{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, a / 3600, a % 3600 / 60)
}

/// A wall time of the NAS's zone (ms already shifted by its offset) as a `js_sys::Date` in UTC.
fn wall(ms: f64) -> js_sys::Date {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms))
}

fn wall_text(ms: f64) -> String {
    i18n::date_with(ms, &[("timeZone", "UTC"), ("dateStyle", "full"), ("timeStyle", "medium")])
}

/// `YYYY-MM-DDTHH:MM:SS` for a `datetime-local` input.
fn wall_input(ms: f64) -> String {
    let d = wall(ms);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        d.get_utc_full_year(),
        d.get_utc_month() + 1,
        d.get_utc_date(),
        d.get_utc_hours(),
        d.get_utc_minutes(),
        d.get_utc_seconds()
    )
}

/// Runs a task, toasts `ok`, reloads the state.
fn task(url: &'static str, body: Option<String>, ok: &'static str, busy: RwSignal<bool>, error: RwSignal<Option<String>>, st: Remote<ClockState>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = crate::tasks::run_task(url, body).await;
        let _ = busy.try_set(false);
        match r {
            Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, t(ok).to_string()),
            Ok(task) => {
                let _ = error.try_set(Some(crate::tasks::task_error(&task)));
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
        st.reload();
    });
}

/// Most zones a search lists (the rest is counted: typing more narrows them).
const MAX_ZONE_HITS: usize = 50;

/// Names of zone cities as people write them in the UI's other languages, and their zone.
/// Only cities that name a zone (Milan is not one); a zone this system does not list is
/// skipped. Matched folded ([`fold`]), shown as written here.
const ZONE_ALIASES: &[(&str, &str)] = &[
    ("Roma", "Europe/Rome"),
    ("Londra", "Europe/London"),
    ("Parigi", "Europe/Paris"),
    ("Berlino", "Europe/Berlin"),
    ("Atene", "Europe/Athens"),
    ("Lisbona", "Europe/Lisbon"),
    ("Varsavia", "Europe/Warsaw"),
    ("Bruxelles", "Europe/Brussels"),
    ("Zurigo", "Europe/Zurich"),
    ("Mosca", "Europe/Moscow"),
    ("Praga", "Europe/Prague"),
    ("Copenaghen", "Europe/Copenhagen"),
    ("Stoccolma", "Europe/Stockholm"),
    ("Lussemburgo", "Europe/Luxembourg"),
    ("Bucarest", "Europe/Bucharest"),
    ("Belgrado", "Europe/Belgrade"),
    ("Pechino", "Asia/Shanghai"),
    ("Gerusalemme", "Asia/Jerusalem"),
    ("Il Cairo", "Africa/Cairo"),
    ("Città del Messico", "America/Mexico_City"),
    ("San Paolo", "America/Sao_Paulo"),
    ("Москва", "Europe/Moscow"),
    ("Калининград", "Europe/Kaliningrad"),
    ("Самара", "Europe/Samara"),
    ("Екатеринбург", "Asia/Yekaterinburg"),
    ("Омск", "Asia/Omsk"),
    ("Новосибирск", "Asia/Novosibirsk"),
    ("Красноярск", "Asia/Krasnoyarsk"),
    ("Иркутск", "Asia/Irkutsk"),
    ("Якутск", "Asia/Yakutsk"),
    ("Владивосток", "Asia/Vladivostok"),
    ("Магадан", "Asia/Magadan"),
    ("Камчатка", "Asia/Kamchatka"),
    ("Минск", "Europe/Minsk"),
    ("Киев", "Europe/Kyiv"),
    ("Киев", "Europe/Kiev"),
    ("Лондон", "Europe/London"),
    ("Париж", "Europe/Paris"),
    ("Берлин", "Europe/Berlin"),
    ("Рим", "Europe/Rome"),
    ("Стамбул", "Europe/Istanbul"),
    ("Токио", "Asia/Tokyo"),
    ("Пекин", "Asia/Shanghai"),
    ("Нью-Йорк", "America/New_York"),
    ("Ташкент", "Asia/Tashkent"),
    ("Алматы", "Asia/Almaty"),
    ("Ереван", "Asia/Yerevan"),
    ("Тбилиси", "Asia/Tbilisi"),
    ("Баку", "Asia/Baku"),
];

/// Text as matching compares it: lowercase, accents off, `_` `-` `/` `,` as spaces, spaces
/// collapsed ("São_Paulo" and "sao paulo" are the same).
fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        let base = match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
            'ç' | 'ć' | 'č' => "c",
            'ď' | 'đ' => "d",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
            'ğ' => "g",
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => "i",
            'ł' | 'ľ' | 'ĺ' => "l",
            'ñ' | 'ń' | 'ň' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ő' => "o",
            'ř' | 'ŕ' => "r",
            'ś' | 'š' | 'ş' | 'ș' => "s",
            'ß' => "ss",
            'ť' | 'ţ' | 'ț' => "t",
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => "u",
            'ý' | 'ÿ' => "y",
            'ź' | 'ž' | 'ż' => "z",
            'æ' => "ae",
            'œ' => "oe",
            'ё' => "е",
            '_' | '-' | '/' | ',' | '.' => " ",
            _ => {
                if c.is_whitespace() {
                    " "
                } else {
                    out.push(c);
                    continue;
                }
            }
        };
        out.push_str(base);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// An offset typed as a search, in seconds: "+01", "+1", "utc+1", "GMT-5", "-05:30", "+0545",
/// "UTC −3". `None` for anything else.
fn offset_query(q: &str) -> Option<i32> {
    let s: String = q.trim().to_lowercase().replace('−', "-").chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.strip_prefix("utc").or_else(|| s.strip_prefix("gmt")).unwrap_or(&s);
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None if rest.len() == 4 => rest.split_at(2),
        None => (rest, "0"),
    };
    let ok = |x: &str, max: usize| !x.is_empty() && x.len() <= max && x.bytes().all(|b| b.is_ascii_digit());
    if !ok(h, 2) || !ok(m, 2) {
        return None;
    }
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// A zone found by a search: its index in the list and the alias it matched by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Hit {
    i: usize,
    alias: Option<&'static str>,
}

/// The zones of `list` that match `q`, best first: the city (or one of its
/// [`ZONE_ALIASES`]) starting with it, then containing it, then every word in the region or
/// the full name; an offset ([`offset_query`]) lists the zones at that offset now. Case,
/// accents, spaces and `_` do not count. Empty `q`: none.
fn zone_matches(list: &[ZoneInfo], q: &str) -> Vec<Hit> {
    if let Some(off) = offset_query(q) {
        return list.iter().enumerate().filter(|(_, z)| z.offset_s == off).map(|(i, _)| Hit { i, alias: None }).collect();
    }
    let qf = fold(q);
    if qf.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = qf.split(' ').collect();
    let squeezed = qf.replace(' ', "");
    let mut hits: Vec<(u8, usize, Hit)> = Vec::new();
    for (i, z) in list.iter().enumerate() {
        let full = fold(&z.name);
        let city = fold(z.name.rsplit('/').next().unwrap_or(&z.name));
        let alias = ZONE_ALIASES
            .iter()
            .filter(|(_, zone)| *zone == z.name)
            .map(|(a, _)| (*a, fold(a)))
            .filter(|(_, f)| f.contains(&qf))
            .min_by_key(|(_, f)| !f.starts_with(&qf));
        let starts = alias.as_ref().is_some_and(|(_, f)| f.starts_with(&qf));
        let alias = alias.map(|(a, _)| a);
        let score = if city.starts_with(&qf) {
            0
        } else if starts {
            1
        } else if city.contains(&qf) || alias.is_some() {
            2
        } else if words.iter().all(|w| full.contains(w)) || full.replace(' ', "").contains(&squeezed) {
            3
        } else {
            continue;
        };
        let alias = alias.filter(|_| score == 1 || (score == 2 && !city.contains(&qf)));
        hits.push((score, i, Hit { i, alias }));
    }
    hits.sort_by_key(|(s, i, _)| (*s, *i));
    hits.into_iter().map(|(_, _, h)| h).collect()
}

/// One option of the zone list.
#[derive(Clone, PartialEq)]
struct ZoneRow {
    name: String,
    /// The last part of the name ("Buenos Aires").
    city: String,
    /// The parts before it ("America / Argentina"), empty for "UTC".
    region: String,
    offset_s: i32,
    /// The alias the search matched by ("Roma").
    alias: Option<&'static str>,
}

impl ZoneRow {
    fn of(z: &ZoneInfo, alias: Option<&'static str>) -> Self {
        let (region, city) = z.name.rsplit_once('/').unwrap_or(("", &z.name));
        ZoneRow {
            name: z.name.clone(),
            city: city.replace('_', " "),
            region: region.replace('_', " ").replace('/', " / "),
            offset_s: z.offset_s,
            alias,
        }
    }
}

/// One zone of the combobox list. `aria-selected` marks the zone in force (`current`); the
/// keyboard and pointer highlight is the field's `aria-activedescendant` and the `is-active`
/// class. `grouped`: under its region's heading, which names the region already.
fn zone_option(
    lid: &str,
    i: usize,
    z: &ZoneRow,
    grouped: bool,
    current: bool,
    active: RwSignal<usize>,
    choose: impl Fn(usize) + Copy + Send + Sync + 'static,
) -> AnyView {
    let city = match z.alias {
        Some(a) => format!("{} · {a}", z.city),
        None => z.city.clone(),
    };
    let region = (!grouped || z.region.contains(" / ")).then(|| z.region.clone());
    view! {
        <li
            id=format!("{lid}-{i}")
            class=move || if active.get() == i { "tz-opt is-active" } else { "tz-opt" }
            role="option"
            aria-selected=current.to_string()
            on:mousemove=move |_| if active.get_untracked() != i { active.set(i) }
            on:click=move |_| choose(i)
        >
            <span class="tz-city">{city}</span>
            {region.map(|r| view! { <span class="tz-region">{r}</span> })}
            <span class="tz-off tnum">{utc(z.offset_s)}</span>
        </li>
    }
    .into_any()
}

fn zone_field(d: RwSignal<Form>, zones: Remote<Vec<ZoneInfo>>) -> AnyView {
    let q = RwSignal::new(String::new());
    let open = RwSignal::new(false);
    let active = RwSignal::new(0usize);
    let id = fid();
    let lid = format!("{id}-list");
    let list = NodeRef::<leptos::html::Div>::new();
    // what the list shows: every zone for an empty search (grouped by region), else the best
    // matches, and how many more matched
    let rows = Memo::new(move |_| {
        let query = q.get();
        zones.data.with(|z| {
            let all = z.as_deref().unwrap_or(&[]);
            if query.trim().is_empty() {
                (all.iter().map(|z| ZoneRow::of(z, None)).collect::<Vec<_>>(), 0)
            } else {
                let hits = zone_matches(all, &query);
                let more = hits.len().saturating_sub(MAX_ZONE_HITS);
                (hits.into_iter().take(MAX_ZONE_HITS).map(|h| ZoneRow::of(&all[h.i], h.alias)).collect(), more)
            }
        })
    });
    let count = move || rows.with(|(r, _)| r.len());
    let selected = move || {
        let cur = d.with(|f| f.tz.clone());
        let off = zones.data.with(|z| z.as_ref().and_then(|z| z.iter().find(|x| x.name == cur).map(|x| x.offset_s)));
        let name = cur.replace('_', " ");
        match off {
            Some(o) => format!("{name} · {}", utc(o)),
            None => name,
        }
    };
    let show = move || {
        q.set(String::new());
        open.set(true);
        // the list starts at the current zone
        let cur = d.with_untracked(|f| f.tz.clone());
        active.set(rows.with_untracked(|(r, _)| r.iter().position(|r| r.name == cur)).unwrap_or(0));
    };
    let hide = move || {
        open.set(false);
        q.set(String::new());
    };
    let choose = move |i: usize| {
        if let Some(name) = rows.with_untracked(|(r, _)| r.get(i).map(|r| r.name.clone())) {
            d.update(|f| f.tz = name);
        }
        hide();
    };
    // keeps the active option in sight inside the list (never scrolls the page)
    let lid2 = lid.clone();
    Effect::new(move || {
        let a = active.get();
        if !open.get() {
            return;
        }
        let lid = lid2.clone();
        request_animation_frame(move || {
            use wasm_bindgen::JsCast;
            let (Some(ul), Some(el)) = (list.get_untracked(), document().get_element_by_id(&format!("{lid}-{a}"))) else { return };
            let Ok(el) = el.dyn_into::<web_sys::HtmlElement>() else { return };
            let (top, h) = (el.offset_top(), el.offset_height());
            let (st, vh) = (ul.scroll_top(), ul.client_height());
            if top < st {
                ul.set_scroll_top(top);
            } else if top + h > st + vh {
                ul.set_scroll_top(top + h - vh);
            }
        });
    });
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let n = count();
        match ev.key().as_str() {
            // the list opens on typing, a click or the down arrow (never on focus alone)
            "ArrowDown" if !open.get_untracked() => {
                ev.prevent_default();
                show();
            }
            "ArrowUp" if !open.get_untracked() => {}
            "ArrowDown" => {
                ev.prevent_default();
                active.update(|a| *a = (*a + 1).min(n.saturating_sub(1)));
            }
            "ArrowUp" => {
                ev.prevent_default();
                active.update(|a| *a = a.saturating_sub(1));
            }
            "PageDown" if open.get_untracked() => {
                ev.prevent_default();
                active.update(|a| *a = (*a + 10).min(n.saturating_sub(1)));
            }
            "PageUp" if open.get_untracked() => {
                ev.prevent_default();
                active.update(|a| *a = a.saturating_sub(10));
            }
            "Enter" if open.get_untracked() => {
                ev.prevent_default();
                if n > 0 {
                    choose(active.get_untracked().min(n - 1));
                }
            }
            "Escape" if open.get_untracked() => {
                ev.prevent_default();
                ev.stop_propagation();
                hide();
            }
            "Tab" => hide(),
            _ => {}
        }
    };
    let options = {
        let lid = lid.clone();
        move || {
            let cur = d.with(|f| f.tz.clone());
            let grouped = q.with(|q| q.trim().is_empty());
            rows.with(|(r, more)| {
                if r.is_empty() {
                    let key = if zones.data.with(Option::is_none) { "loading" } else { "time.zone_none" };
                    return view! { <div class="tz-empty" role="presentation">{t(key)}</div> }.into_any();
                }
                let opt = |i: usize| zone_option(&lid, i, &r[i], grouped, r[i].name == cur, active, choose);
                let body = if grouped {
                    // the regions as option groups, named by their heading
                    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
                    for (i, z) in r.iter().enumerate() {
                        let top = z.name.split('/').next().unwrap_or("");
                        match groups.last_mut() {
                            Some((g, v)) if g == top => v.push(i),
                            _ => groups.push((top.to_string(), vec![i])),
                        }
                    }
                    groups
                        .into_iter()
                        .enumerate()
                        .map(|(gi, (g, idx))| {
                            let gid = format!("{lid}-g{gi}");
                            let head_id = gid.clone();
                            let items = idx.into_iter().map(opt).collect_view();
                            view! {
                                <ul class="tz-grp" role="group" aria-labelledby=gid>
                                    <li id=head_id class="tz-group" role="presentation">{g.replace('_', " ")}</li>
                                    {items}
                                </ul>
                            }
                        })
                        .collect_view()
                        .into_any()
                } else {
                    let items = (0..r.len()).map(opt).collect_view();
                    view! { <ul class="tz-grp" role="presentation">{items}</ul> }.into_any()
                };
                let more = (*more > 0).then(|| view! { <div class="tz-more" role="presentation">{tf("time.zone_more", &[("n", &more.to_string())])}</div> });
                view! { {body}{more} }.into_any()
            })
        }
    };
    let lid3 = lid.clone();
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr("ps.time_zone")}</label>
            <div class="tz-combo">
                <input
                    id=id
                    class="input"
                    type="text"
                    role="combobox"
                    autocomplete="off"
                    spellcheck="false"
                    aria-autocomplete="list"
                    aria-expanded=move || open.get().to_string()
                    aria-controls=lid.clone()
                    aria-activedescendant=move || (open.get() && count() > 0).then(|| format!("{lid3}-{}", active.get()))
                    placeholder=move || if open.get() { selected() } else { t("time.zone_search").to_string() }
                    prop:value=move || if open.get() { q.get() } else { selected() }
                    on:click=move |_| if !open.get_untracked() { show() }
                    on:blur=move |_| hide()
                    on:input=move |ev| {
                        q.set(event_target_value(&ev));
                        open.set(true);
                        active.set(0);
                    }
                    on:keydown=on_key
                />
                <Show when=move || open.get()>
                    // a press inside the list keeps the focus in the field (no blur before the click)
                    <div id=lid.clone() node_ref=list class="tz-list" role="listbox" aria-label=tr("ps.time_zone") on:mousedown=|ev| ev.prevent_default()>
                        {options.clone()}
                    </div>
                </Show>
            </div>
            <span class="field-help">{tr("time.zone_help")}</span>
        </div>
    }
    .into_any()
}

fn form_card(init: ClockState, st: Remote<ClockState>, zones: Remote<Vec<ZoneInfo>>) -> AnyView {
    let draft = Draft::new(form_of(&init), move || st.data.with(|s| s.as_ref().map(form_of)));
    let d = draft.d;
    let dirty = Signal::derive(move || draft.base.with(|b| d.with(|f| f.tz != b.tz || f.sync != b.sync || servers(&f.servers) != servers(&b.servers))));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let sync = Signal::derive(move || d.with(|f| f.sync));
    let text = Signal::derive(move || d.with(|f| f.servers.clone()));
    let valid = Signal::derive(move || d.with(|f| servers(&f.servers).len() <= MAX_SERVERS));
    let defaults = move || {
        let own = d.with(|f| !servers(&f.servers).is_empty());
        st.data.with(|s| {
            let s = s.as_ref()?;
            let mut v = Vec::new();
            if !s.default_servers.is_empty() {
                v.push(tf("time.defaults", &[("list", &s.default_servers.join(", "))]));
            }
            if !s.dhcp_servers.is_empty() {
                // timesyncd asks the servers of the interfaces before any other
                let key = if s.daemon == TimeDaemon::Timesyncd { "time.dhcp_first" } else { "time.dhcp" };
                v.push(tf(key, &[("list", &s.dhcp_servers.join(", "))]));
            }
            if !s.dhcp_hook_servers.is_empty() {
                let key = if own { "time.dhcp_replaced" } else { "time.dhcp" };
                v.push(tf(key, &[("list", &s.dhcp_hook_servers.join(", "))]));
            }
            if s.daemon == TimeDaemon::Chrony {
                v.push(t("time.also_own").to_string());
            }
            (!v.is_empty()).then(|| view! { <p class="muted small">{v.join(" ")}</p> })
        })
    };
    let ignored = move || {
        let own = d.with(|f| !servers(&f.servers).is_empty());
        let off = st.data.with(|s| s.as_ref().is_some_and(|s| s.servers_ignored));
        (own && off).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("time.servers_ignored")} }.into_any()))
    };
    let other = Signal::derive(move || st.data.with(|s| s.as_ref().is_some_and(|s| s.daemon == TimeDaemon::Other)));
    let needs_install = Signal::derive(move || sync.get() && st.data.with(|s| s.as_ref().is_some_and(|s| s.daemon == TimeDaemon::None)));
    let save = move |_| {
        let rtc = st.data.with_untracked(|s| s.as_ref().is_some_and(|s| s.local_rtc));
        let body = serde_json::to_string(&settings(&d.get_untracked(), rtc)).unwrap_or_default();
        task(URL, Some(body), "time.saved", busy, error, st);
    };
    view! {
        <section class="card widget w-6" style="--i:0">
            <h2 class="section-title">{tr("time.settings")}</h2>
            {stale_notice(draft.stale, Callback::new(move |_| draft.reset()))}
            {zone_field(d, zones)}
            <Show when=move || !other.get() fallback=|| view! { <p class="muted small">{tr("time.other_daemon")}</p> }>
                {switch("time.sync", Some("time.sync_help"), sync, Callback::new(move |v| d.update(|f| f.sync = v)))}
                <Show when=move || sync.get()>
                    {area_field("time.servers", text, Callback::new(move |v| d.update(|f| f.servers = v)), Some("time.servers_help"), "ntp.example.lan")}
                    {ignored}
                    {defaults}
                </Show>
            </Show>
            <Show when=move || needs_install.get()>
                <p class="muted small">{tr("time.will_install")}</p>
            </Show>
            {form_error(error)}
            <div class="card-actions">
                <button type="button" class="btn ghost" disabled=move || busy.get() || !dirty.get() on:click=move |_| draft.reset()>{tr("cancel")}</button>
                <button type="button" class="btn primary" disabled=move || busy.get() || !dirty.get() || !valid.get() on:click=save>
                    {spinner(busy)}{tr("time.apply")}
                </button>
            </div>
        </section>
    }
    .into_any()
}

fn kv(label: &'static str, value: String) -> AnyView {
    ui::kv_text(label, value, "tnum")
}

/// The clock now (ticking) and what the time service says.
fn status_card(st: Remote<ClockState>) -> AnyView {
    // the tick only says when to draw again: the time is the server's answer plus this
    // browser's clock since it came, right after a reload and in a throttled background tab
    let tick = crate::live::use_live().uptime_offset;
    let anchor = Memo::new(move |_| st.data.with(|s| s.as_ref().map(|s| (s.now_ms as f64 + s.utc_offset_s as f64 * 1000.0, js_sys::Date::now()))));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let clock = move || {
        tick.track();
        anchor.get().map(|(wall_ms, received)| wall_text(wall_ms + (js_sys::Date::now() - received)))
    };
    let rows = move || {
        st.data.with(|s| {
            let s = s.as_ref()?;
            let (tone, key) = match (s.sync_enabled, s.synchronized) {
                (false, _) => (Tone::Unknown, "time.st_off"),
                (true, true) => (Tone::Ok, "time.st_synced"),
                (true, false) => (Tone::Warn, "time.st_unsynced"),
            };
            let mut v = vec![
                kv("ps.time_zone", format!("{} ({})", s.timezone, utc(s.utc_offset_s))),
                view! { <div class="kv"><dt>{tr("time.state")}</dt><dd>{ui::status(tone, t(key).to_string())}</dd></div> }.into_any(),
            ];
            if let Some(x) = s.source.as_ref() {
                let server = match &x.address {
                    Some(a) if *a != x.server => format!("{} ({a})", x.server),
                    _ => x.server.clone(),
                };
                v.push(kv("time.source", server));
                if let Some(o) = x.offset_us {
                    v.push(kv("time.offset", format!("{} ms", i18n::num(o as f64 / 1000.0, 1))));
                }
                if let Some(at) = x.last_sync_ms {
                    v.push(kv("time.last", i18n::datetime(at)));
                }
                if let Some(p) = x.poll_s {
                    v.push(kv("time.poll", i18n::duration(u64::from(p))));
                }
            }
            Some(v)
        })
    };
    let can_sync = Signal::derive(move || st.data.with(|s| s.as_ref().is_some_and(|s| s.sync_enabled && s.daemon != TimeDaemon::Other)));
    view! {
        <section class="card widget w-6" style="--i:1">
            <h2 class="section-title">{tr("time.status")}</h2>
            <p class="time-now tnum">{clock}</p>
            <dl class="kvs">{rows}</dl>
            {form_error(error)}
            <Show when=move || can_sync.get()>
                <div class="card-actions">
                    <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| task("/api/v1/system/time/sync-now", None, "time.synced", busy, error, st)>
                        {spinner(busy)}{tr("time.sync_now")}
                    </button>
                </div>
            </Show>
        </section>
    }
    .into_any()
}

/// Date and time by hand, only while automatic sync is off.
fn manual_card(st: Remote<ClockState>) -> AnyView {
    let init = st.data.with_untracked(|s| s.as_ref().map(|s| wall_input(s.now_ms as f64 + s.utc_offset_s as f64 * 1000.0))).unwrap_or_default();
    let value = RwSignal::new(init);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let id = fid();
    let send = move |req: SetClock| task("/api/v1/system/time/set", serde_json::to_string(&req).ok(), "time.set_done", busy, error, st);
    view! {
        <section class="card widget w-6" style="--i:2">
            <h2 class="section-title">{tr("time.manual")}</h2>
            <div class="field">
                <label class="field-label" for=id.clone()>{tr("time.manual_label")}</label>
                <input id=id class="input" type="datetime-local" step="1" min=format!("{MIN_YEAR}-01-01T00:00:00") max=format!("{MAX_YEAR}-12-31T23:59:59") prop:value=move || value.get() on:input=move |ev| value.set(event_target_value(&ev)) />
            </div>
            {form_error(error)}
            <div class="card-actions">
                <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| send(SetClock { epoch_ms: Some(js_sys::Date::now() as u64), local: None })>
                    {tr("time.browser")}
                </button>
                <button type="button" class="btn primary" disabled=move || busy.get() || value.with(String::is_empty) on:click=move |_| {
                    let v = value.get_untracked().replace('T', " ");
                    send(SetClock { epoch_ms: None, local: Some(v) });
                }>{spinner(busy)}{tr("time.set")}</button>
            </div>
        </section>
    }
    .into_any()
}

/// A hardware clock in local time, with the move to UTC.
fn rtc_note(st: Remote<ClockState>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let fix = move |_| {
        let Some(s) = st.data.get_untracked() else { return };
        let body = serde_json::to_string(&settings(&form_of(&s), false)).unwrap_or_default();
        task(URL, Some(body), "time.saved", busy, error, st);
    };
    view! {
        <Show when=move || st.data.with(|s| s.as_ref().is_some_and(|s| s.local_rtc))>
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
                {tr("time.local_rtc")}" "
                <button type="button" class="link-btn" disabled=move || busy.get() on:click=fix>{tr("time.rtc_utc")}</button>
            }.into_any())}
            {form_error(error)}
        </Show>
    }
    .into_any()
}

#[component]
pub fn TimeSettings() -> impl IntoView {
    let st = Remote::<ClockState>::new(URL);
    let zones = Remote::<Vec<ZoneInfo>>::new("/api/v1/system/time/zones");
    let manual = Memo::new(move |_| st.data.with(|s| s.as_ref().is_some_and(|s| !s.sync_enabled)));
    let body = move || {
        let Some(s) = st.data.get_untracked() else {
            return ui::loading(st.err.into(), Callback::new(move |_| st.reload()), 6);
        };
        view! {
            {rtc_note(st)}
            <div class="grid">
                {form_card(s, st, zones)}
                {status_card(st)}
                {move || manual.get().then(|| manual_card(st))}
            </div>
        }
        .into_any()
    };
    // built once, when the first answer comes: later answers update it through its draft
    let first = Memo::new(move |_| st.data.with(Option::is_some));
    view! {
        <div class="page">
            <PageHeader icon=icons::CALENDAR_CLOCK title="time.title" subtitle="time.subtitle" />
            {move || { first.get(); body() }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_servers() {
        assert_eq!(utc(3600), "UTC+01:00");
        assert_eq!(utc(-16200), "UTC-04:30");
        assert_eq!(utc(0), "UTC+00:00");
        assert_eq!(servers(" a.example\n\nb.example, c.example "), ["a.example", "b.example", "c.example"]);
    }

    fn zones() -> Vec<ZoneInfo> {
        [
            ("Africa/Cairo", 7200),
            ("America/Argentina/Buenos_Aires", -10800),
            ("America/New_York", -18000),
            ("America/Sao_Paulo", -10800),
            ("Asia/Kathmandu", 20700),
            ("Asia/Kolkata", 19800),
            ("Asia/Shanghai", 28800),
            ("Europe/Kyiv", 7200),
            ("Europe/Moscow", 10800),
            ("Europe/Rome", 3600),
            ("Europe/Zurich", 3600),
            ("Pacific/Kiritimati", 50400),
            ("UTC", 0),
        ]
        .into_iter()
        .map(|(name, offset_s)| ZoneInfo { name: name.into(), offset_s })
        .collect()
    }

    fn names(q: &str) -> Vec<String> {
        let z = zones();
        zone_matches(&z, q).into_iter().map(|h| z[h.i].name.clone()).collect()
    }

    #[test]
    fn folding_and_offsets() {
        assert_eq!(fold("  São_Paulo "), "sao paulo");
        assert_eq!(fold("Città  del-Messico"), "citta del messico");
        assert_eq!(fold("Нью-Йорк"), "нью йорк");
        assert_eq!(fold("Ёлки"), "елки");
        assert_eq!(offset_query("+01"), Some(3600));
        assert_eq!(offset_query("+1"), Some(3600));
        assert_eq!(offset_query("UTC+1"), Some(3600));
        assert_eq!(offset_query("gmt -5"), Some(-18000));
        assert_eq!(offset_query("utc−3"), Some(-10800));
        assert_eq!(offset_query("+05:30"), Some(19800));
        assert_eq!(offset_query("+0545"), Some(20700));
        assert_eq!(offset_query("-05:30"), Some(-19800));
        for no in ["", "utc", "rome", "+", "+1:2:3", "+15", "+01:60", "1", "+abc"] {
            assert_eq!(offset_query(no), None, "{no}");
        }
    }

    #[test]
    fn zone_search() {
        // case, accents, spaces for underscores, city before region
        assert_eq!(names("rome"), ["Europe/Rome"]);
        assert_eq!(names("ROME"), ["Europe/Rome"]);
        assert_eq!(names("new york"), ["America/New_York"]);
        assert_eq!(names("new_york"), ["America/New_York"]);
        assert_eq!(names("newyork"), ["America/New_York"]);
        assert_eq!(names("são paulo"), ["America/Sao_Paulo"]);
        assert_eq!(names("buenos"), ["America/Argentina/Buenos_Aires"]);
        assert_eq!(names("argentina"), ["America/Argentina/Buenos_Aires"]);
        assert_eq!(names("europe rome"), ["Europe/Rome"]);
        // the city first, then the region
        assert_eq!(names("ka")[..2], ["Asia/Kathmandu".to_string(), "Asia/Kolkata".to_string()]);
        assert_eq!(names("europe").len(), 4);
        // names in Italian and Russian, with the alias shown
        let z = zones();
        let h = zone_matches(&z, "Roma");
        assert_eq!((z[h[0].i].name.as_str(), h[0].alias), ("Europe/Rome", Some("Roma")));
        assert_eq!(names("mosca"), ["Europe/Moscow"]);
        assert_eq!(names("москва"), ["Europe/Moscow"]);
        assert_eq!(names("Нью Йорк"), ["America/New_York"]);
        assert_eq!(names("zurigo"), ["Europe/Zurich"]);
        assert_eq!(names("pechino"), ["Asia/Shanghai"]);
        assert_eq!(names("citta del messico"), Vec::<String>::new(), "a zone this list lacks is skipped");
        assert_eq!(names("киев"), ["Europe/Kyiv"]);
        // a match by the English name shows no alias
        assert_eq!(zone_matches(&z, "rom")[0].alias, None);
        // offsets
        assert_eq!(names("+01"), ["Europe/Rome", "Europe/Zurich"]);
        assert_eq!(names("utc-3"), ["America/Argentina/Buenos_Aires", "America/Sao_Paulo"]);
        assert_eq!(names("+05:45"), ["Asia/Kathmandu"]);
        assert_eq!(names("+14"), ["Pacific/Kiritimati"]);
        assert_eq!(names("utc"), ["UTC"]);
        assert!(names("   ").is_empty());
        assert!(names("atlantis").is_empty());
    }

    #[test]
    fn zone_rows() {
        let r = ZoneRow::of(&ZoneInfo { name: "America/Argentina/Buenos_Aires".into(), offset_s: -10800 }, None);
        assert_eq!((r.city.as_str(), r.region.as_str()), ("Buenos Aires", "America / Argentina"));
        let r = ZoneRow::of(&ZoneInfo { name: "UTC".into(), offset_s: 0 }, None);
        assert_eq!((r.city.as_str(), r.region.as_str()), ("UTC", ""));
    }
}
