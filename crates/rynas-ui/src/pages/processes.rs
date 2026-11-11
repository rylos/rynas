//! Sistema › Processi (D-PROCESSES, `docs/research/processes.md`): every process grouped by
//! service, container (app), session; a flat and a tree view sortable by CPU, memory, disk
//! I/O and age; a filter; a drawer with the details, a CPU/memory graph of the selected
//! process (kept here, at most [`HISTORY`] points) and the actions: restart the service or
//! container, priority, terminate, kill. The list is asked for every [`POLL_MS`] while the
//! page is visible (the server samples only then).

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::processes::{
    GroupKind, IoClass, PriorityRequest, ProcSignal, ProcState, ProcessDetail, ProcessGroup, ProcessInfo, ProcessList, RestartRequest,
    SignalRequest,
};

use crate::api;
use crate::chart::StillChart;
use crate::form::{search_box, segmented, select_field, spinner, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, Confirm, ErrorBox, PageHeader, Skeleton, Tone};

const URL: &str = "/api/v1/processes";
const POLL_MS: u32 = 2000;
/// Points of the drawer's graph: 5 minutes at one every 2 s.
const HISTORY: usize = 150;

const VIEWS: &[(&str, &str)] = &[("groups", "proc.view_groups"), ("list", "proc.view_list"), ("tree", "proc.view_tree")];
const SORTS: &[(&str, &str)] = &[
    ("cpu", "proc.sort_cpu"),
    ("mem", "proc.sort_mem"),
    ("read", "proc.sort_read"),
    ("write", "proc.sort_write"),
    ("age", "proc.sort_age"),
    ("name", "proc.sort_name"),
];
/// Named priority levels: (nice, i18n key).
const LEVELS: &[(&str, &str)] = &[("-10", "proc.prio_high"), ("-5", "proc.prio_above"), ("0", "proc.prio_normal"), ("10", "proc.prio_low"), ("19", "proc.prio_lowest")];
const IO: &[(&str, &str)] = &[("best_effort", "proc.io_normal"), ("idle", "proc.io_idle")];

/// A process as the key of a row and of the actions.
fn pkey(p: &ProcessInfo) -> String {
    format!("p{}:{}", p.pid, p.start_ms)
}

/// One row of the table, ready to show.
#[derive(Clone, PartialEq, Default)]
struct Row {
    group: bool,
    depth: u8,
    name: String,
    sub: String,
    user: String,
    cpu: Option<f32>,
    mem: u64,
    read: Option<u64>,
    write: Option<u64>,
    count: usize,
    open: bool,
    gid: String,
    pid: u32,
    start_ms: u64,
}

#[derive(Default, Clone, Copy)]
struct Sum {
    cpu: f32,
    mem: u64,
    read: Option<u64>,
    write: Option<u64>,
    count: usize,
    oldest: u64,
}

fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        _ => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// What the group is called on screen.
fn group_title(g: &ProcessGroup) -> String {
    match g.kind {
        GroupKind::Container => g.app.clone().unwrap_or_else(|| g.name.clone()),
        GroupKind::Session => tf("proc.g.session", &[("user", &g.name)]),
        GroupKind::User => tf("proc.g.user", &[("user", &g.name)]),
        GroupKind::Kernel => t("proc.g.kernel").to_string(),
        _ => g.name.clone(),
    }
}

fn kind_key(k: GroupKind) -> &'static str {
    match k {
        GroupKind::Service => "proc.k.service",
        GroupKind::Container => "proc.k.container",
        GroupKind::Session => "proc.k.session",
        GroupKind::User => "proc.k.user",
        GroupKind::Kernel => "proc.k.kernel",
        GroupKind::System => "proc.k.system",
    }
}

fn group_sub(g: &ProcessGroup) -> String {
    match (g.kind, &g.app) {
        (GroupKind::Container, Some(_)) => format!("{} · {}", t("proc.k.app"), g.name),
        _ => t(kind_key(g.kind)).to_string(),
    }
}

fn state_key(s: ProcState) -> &'static str {
    match s {
        ProcState::Running => "proc.st.running",
        ProcState::Sleeping => "proc.st.sleeping",
        ProcState::DiskWait => "proc.st.disk_wait",
        ProcState::Stopped => "proc.st.stopped",
        ProcState::Zombie => "proc.st.zombie",
        ProcState::Idle => "proc.st.idle",
        ProcState::Other => "proc.st.other",
    }
}

/// Sort key, larger first (name and age: as read).
fn sort_val(sort: &str, cpu: f32, mem: u64, read: Option<u64>, write: Option<u64>, start: u64) -> f64 {
    match sort {
        "mem" => mem as f64,
        "read" => read.unwrap_or(0) as f64,
        "write" => write.unwrap_or(0) as f64,
        // oldest first
        "age" => -(start as f64),
        _ => cpu as f64,
    }
}

fn matches(q: &str, p: &ProcessInfo, g: &ProcessGroup) -> bool {
    q.is_empty()
        || [p.name.as_str(), p.cmd.as_str(), p.user.as_str(), g.name.as_str(), g.app.as_deref().unwrap_or("")]
            .iter()
            .any(|s| s.to_lowercase().contains(q))
        || p.pid.to_string() == q
}

fn proc_row(p: &ProcessInfo, depth: u8) -> Row {
    Row {
        depth,
        name: p.name.clone(),
        sub: if p.cmd.is_empty() { format!("PID {}", p.pid) } else { format!("{} · {}", p.pid, p.cmd) },
        user: p.user.clone(),
        cpu: p.cpu_pct,
        mem: p.rss_bytes,
        read: p.read_bytes_s,
        write: p.write_bytes_s,
        pid: p.pid,
        start_ms: p.start_ms,
        ..Row::default()
    }
}

/// The rows in order for a view, sort, filter and the open groups.
fn build(l: &ProcessList, view: &str, sort: &str, q: &str, open: &HashSet<String>) -> Vec<(String, Row)> {
    let q = q.trim().to_lowercase();
    let group = |p: &ProcessInfo| &l.groups[(p.group as usize).min(l.groups.len().saturating_sub(1))];
    let shown: Vec<&ProcessInfo> = l.processes.iter().filter(|p| !l.groups.is_empty() && matches(&q, p, group(p))).collect();
    let by = |a: &&ProcessInfo, b: &&ProcessInfo| {
        if sort == "name" {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        } else {
            let k = |p: &ProcessInfo| sort_val(sort, p.cpu_pct.unwrap_or(0.0), p.rss_bytes, p.read_bytes_s, p.write_bytes_s, p.start_ms);
            k(b).total_cmp(&k(a))
        }
    };
    let mut out = Vec::new();
    match view {
        "list" => {
            let mut v = shown;
            v.sort_by(by);
            out.extend(v.into_iter().map(|p| (pkey(p), proc_row(p, 0))));
        }
        "tree" => {
            // matches and their ancestors, children under their parent
            let all: HashMap<u32, &ProcessInfo> = l.processes.iter().map(|p| (p.pid, p)).collect();
            let mut keep: HashSet<u32> = HashSet::new();
            for p in &shown {
                let mut cur = Some(*p);
                while let Some(c) = cur {
                    if !keep.insert(c.pid) {
                        break;
                    }
                    cur = all.get(&c.ppid).copied().filter(|x| x.pid != c.pid);
                }
            }
            let mut kids: HashMap<u32, Vec<&ProcessInfo>> = HashMap::new();
            let mut roots = Vec::new();
            for p in l.processes.iter().filter(|p| keep.contains(&p.pid)) {
                if keep.contains(&p.ppid) && p.ppid != p.pid {
                    kids.entry(p.ppid).or_default().push(p);
                } else {
                    roots.push(p);
                }
            }
            roots.sort_by(by);
            let mut stack: Vec<(&ProcessInfo, u8)> = roots.into_iter().rev().map(|p| (p, 0)).collect();
            while let Some((p, d)) = stack.pop() {
                out.push((pkey(p), proc_row(p, d)));
                if let Some(mut ch) = kids.remove(&p.pid) {
                    ch.sort_by(by);
                    stack.extend(ch.into_iter().rev().map(|c| (c, d.saturating_add(1))));
                }
            }
        }
        _ => {
            let mut sums: Vec<Sum> = vec![Sum::default(); l.groups.len()];
            let mut members: Vec<Vec<&ProcessInfo>> = vec![Vec::new(); l.groups.len()];
            for p in &shown {
                let i = (p.group as usize).min(l.groups.len() - 1);
                let s = &mut sums[i];
                s.cpu += p.cpu_pct.unwrap_or(0.0);
                s.mem += p.rss_bytes;
                s.read = add(s.read, p.read_bytes_s);
                s.write = add(s.write, p.write_bytes_s);
                s.count += 1;
                s.oldest = if s.oldest == 0 { p.start_ms } else { s.oldest.min(p.start_ms) };
                members[i].push(p);
            }
            let mut order: Vec<usize> = (0..l.groups.len()).filter(|&i| sums[i].count > 0).collect();
            order.sort_by(|&a, &b| {
                if sort == "name" {
                    group_title(&l.groups[a]).to_lowercase().cmp(&group_title(&l.groups[b]).to_lowercase())
                } else {
                    let k = |i: usize| {
                        let s = sums[i];
                        sort_val(sort, s.cpu, s.mem, s.read, s.write, s.oldest)
                    };
                    k(b).total_cmp(&k(a))
                }
            });
            for i in order {
                let g = &l.groups[i];
                let s = sums[i];
                let is_open = open.contains(&g.id) || !q.is_empty();
                out.push((
                    format!("g{}", g.id),
                    Row {
                        group: true,
                        name: group_title(g),
                        sub: group_sub(g),
                        cpu: Some(s.cpu),
                        mem: s.mem,
                        read: s.read,
                        write: s.write,
                        count: s.count,
                        open: is_open,
                        gid: g.id.clone(),
                        ..Row::default()
                    },
                ));
                if is_open {
                    let mut m = std::mem::take(&mut members[i]);
                    m.sort_by(by);
                    out.extend(m.into_iter().map(|p| (pkey(p), proc_row(p, 1))));
                }
            }
        }
    }
    out
}

fn opt_rate(v: Option<u64>) -> String {
    v.map_or_else(|| "—".to_string(), i18n::rate)
}

fn cpu_text(v: Option<f32>) -> String {
    v.map_or_else(|| "—".to_string(), |c| i18n::pct(c as f64))
}

/// One reading of the list; false once the page is gone.
async fn fetch(data: RwSignal<Option<ProcessList>>, err: RwSignal<Option<String>>, kernel: RwSignal<bool>) -> bool {
    let Some(k) = kernel.try_get_untracked() else { return false };
    let url = if k { format!("{URL}?kernel=true") } else { URL.to_string() };
    // `try_set` gives the value back when the signal is gone (the page was left)
    match api::get::<ProcessList>(&url).await {
        Ok(l) => {
            if data.try_set(Some(l)).is_some() {
                return false;
            }
            let _ = err.try_set(None);
        }
        // the API layer shows the login screen
        Err(e) if e.status() == 401 => {}
        Err(e) => {
            if err.try_set(Some(e.code().to_string())).is_some() {
                return false;
            }
        }
    }
    true
}

/// Reads the list every [`POLL_MS`] while the page is mounted and the tab visible.
fn poll(data: RwSignal<Option<ProcessList>>, err: RwSignal<Option<String>>, kernel: RwSignal<bool>) {
    spawn_local(async move {
        loop {
            if !document().hidden() && !fetch(data, err, kernel).await {
                break;
            }
            api::sleep(POLL_MS).await;
            if data.try_with_untracked(|_| ()).is_none() {
                break;
            }
        }
    });
}

#[component]
pub fn Processes() -> impl IntoView {
    let data = RwSignal::new(None::<ProcessList>);
    let err = RwSignal::new(None::<String>);
    let kernel = RwSignal::new(false);
    let view_mode = RwSignal::new("groups".to_string());
    let sort = RwSignal::new("cpu".to_string());
    let q = RwSignal::new(String::new());
    let open = RwSignal::new(HashSet::<String>::new());
    let selected = RwSignal::new(None::<(u32, u64)>);
    poll(data, err, kernel);

    let rows = Memo::new(move |_| {
        data.with(|d| {
            d.as_ref()
                .map(|l| view_mode.with(|v| sort.with(|s| q.with(|q| open.with(|o| build(l, v, s, q, o))))))
                .unwrap_or_default()
        })
    });
    let ids = Signal::derive(move || rows.with(|r| r.iter().map(|x| x.0.clone()).collect::<Vec<_>>()));
    let by_key = Memo::new(move |_| rows.with(|r| r.iter().cloned().collect::<HashMap<String, Row>>()));

    let row_view = Callback::new(move |key: String| {
        let k = StoredValue::new(key);
        let get = move |f: fn(&Row) -> String| move || by_key.with(|m| k.with_value(|k| m.get(k).map(f).unwrap_or_default()));
        let is_group = by_key.with_untracked(|m| k.with_value(|k| m.get(k).is_some_and(|r| r.group)));
        // the same process is at another depth in another view (rows are kept by key)
        let depth = move || by_key.with(|m| k.with_value(|k| m.get(k).map_or(0, |r| r.depth.min(8))));
        let toggle = move |_| {
            let r = by_key.with_untracked(|m| k.with_value(|k| m.get(k).cloned())).unwrap_or_default();
            if r.group {
                open.update(|o| {
                    if !o.remove(&r.gid) {
                        o.insert(r.gid.clone());
                    }
                });
            } else {
                selected.set(Some((r.pid, r.start_ms)));
            }
        };
        let open_now = move || by_key.with(|m| k.with_value(|k| m.get(k).is_some_and(|r| r.open)));
        let name_cell = view! {
            <div class="proc-name" style=move || format!("--depth:{}", depth())>
                <button
                    type="button"
                    class=if is_group { "proc-btn group" } else { "proc-btn" }
                    aria-expanded=move || is_group.then(|| if open_now() { "true" } else { "false" })
                    on:click=toggle
                >
                    {is_group.then(|| view! { <span class="proc-chev" class:open=open_now><Icon icon=icons::CHEVRON_RIGHT size=14 /></span> })}
                    <span class="cell-main">
                        <span class="proc-title">
                            {get(|r| r.name.clone())}
                            {is_group.then(|| view! { <span class="chip">{get(|r| r.count.to_string())}</span> })}
                        </span>
                        <span class=if is_group { "cell-sub proc-cmd" } else { "cell-sub mono proc-cmd" }>{get(|r| r.sub.clone())}</span>
                    </span>
                </button>
            </div>
        }
        .into_any();
        let cells = vec![
            ui::cell("proc.col.name", "wrap", name_cell),
            ui::cell("proc.col.user", "", view! { <span>{get(|r| r.user.clone())}</span> }.into_any()),
            ui::cell("proc.col.cpu", "num tnum", view! { <span>{get(|r| cpu_text(r.cpu))}</span> }.into_any()),
            ui::cell("proc.col.memory", "num tnum", view! { <span>{get(|r| i18n::bytes(r.mem))}</span> }.into_any()),
            ui::cell(
                "proc.col.disk",
                "num tnum",
                view! {
                    <span class="proc-io">
                        <span title=tr("proc.read")><Icon icon=icons::ARROW_DOWN size=12 />{get(|r| opt_rate(r.read))}</span>
                        <span title=tr("proc.write")><Icon icon=icons::ARROW_UP size=12 />{get(|r| opt_rate(r.write))}</span>
                    </span>
                }
                .into_any(),
            ),
        ];
        if is_group { ui::row_class(cells, "proc-group") } else { ui::row(cells) }
    });

    let summary = move || {
        data.with(|d| {
            d.as_ref().map(|l| {
                let used: u64 = l.processes.iter().map(|p| p.rss_bytes).sum();
                tf(
                    "proc.summary",
                    &[
                        ("n", &i18n::count(l.processes.len() as u64)),
                        ("cpu", &cpu_text(l.cpu_pct)),
                        ("cores", &l.cpus.to_string()),
                        ("mem", &i18n::bytes(used)),
                    ],
                )
            })
        })
    };

    let body = move || {
        if data.with(Option::is_none) {
            return ui::loading(
                err.into(),
                Callback::new(move |_| {
                    spawn_local(async move {
                        fetch(data, err, kernel).await;
                    })
                }),
                8,
            );
        }
        view! {
            <p class="muted small proc-summary tnum">{summary}</p>
            {move || rows.with(|r| r.is_empty()).then(|| view! { <p class="muted table-none">{tr("proc.none")}</p> })}
            {ui::table_body("proc-table", &["proc.col.name", "proc.col.user", "#proc.col.cpu", "#proc.col.memory", "#proc.col.disk"], ui::keyed(ids, row_view))}
        }
        .into_any()
    };
    let first = Memo::new(move |_| data.with(Option::is_some));

    view! {
        <div class="page">
            <PageHeader icon=icons::ACTIVITY title="proc.title" subtitle="proc.subtitle" />
            <section class="card widget w-12">
                <div class="toolbar proc-toolbar">
                    {search_box("proc.search", q)}
                    {segmented("proc.view", VIEWS, view_mode.into(), Callback::new(move |v| view_mode.set(v)))}
                    {select_field("proc.sort", None, SORTS, sort.into(), Callback::new(move |v| sort.set(v)))}
                    {switch("proc.k.kernel", None, kernel.into(), Callback::new(move |v| kernel.set(v)))}
                </div>
                {move || { first.get(); body() }}
            </section>
            {move || selected.get().map(|(pid, start)| detail_drawer(pid, start, data, Callback::new(move |_| selected.set(None))))}
        </div>
    }
}

/// What the confirmation dialog of the drawer asks.
#[derive(Clone, Copy, PartialEq)]
enum Act {
    Term,
    Kill,
    RestartService,
    RestartContainer,
}

fn detail_drawer(pid: u32, start_ms: u64, data: RwSignal<Option<ProcessList>>, close: Callback<()>) -> AnyView {
    let detail = RwSignal::new(None::<Result<ProcessDetail, String>>);
    let load = move || {
        spawn_local(async move {
            let r = api::get::<ProcessDetail>(&format!("{URL}/{pid}?start_ms={start_ms}")).await;
            let _ = detail.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    load();
    // the live numbers come from the list; the graph keeps the last HISTORY of them
    let live = Memo::new(move |_| {
        data.with(|d| d.as_ref().and_then(|l| l.processes.iter().find(|p| p.pid == pid && p.start_ms == start_ms).map(|p| (l.read_at_ms, p.clone()))))
    });
    let hist = RwSignal::new((Vec::<f64>::new(), Vec::<f32>::new(), Vec::<f32>::new()));
    Effect::new(move || {
        if let Some((ts, p)) = live.get() {
            hist.update(|(t, c, m)| {
                if t.last().is_some_and(|l| *l >= ts as f64) {
                    return;
                }
                t.push(ts as f64);
                c.push(p.cpu_pct.unwrap_or(0.0));
                m.push(p.rss_bytes as f32);
                if t.len() > HISTORY {
                    t.remove(0);
                    c.remove(0);
                    m.remove(0);
                }
            });
        }
    });
    let cpu_series = Signal::derive(move || hist.with(|(t, c, _)| (t.clone(), vec![c.clone()])));
    let mem_series = Signal::derive(move || hist.with(|(t, _, m)| (t.clone(), vec![m.clone()])));
    let gone = Memo::new(move |_| data.with(|d| d.is_some()) && live.with(Option::is_none));
    let ask = RwSignal::new(None::<Act>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let title = Signal::derive(move || {
        live.with(|l| l.as_ref().map(|(_, p)| p.name.clone()))
            .or_else(|| detail.with(|d| d.as_ref().and_then(|d| d.as_ref().ok()).map(|d| d.info.name.clone())))
            .map_or_else(|| format!("PID {pid}"), |n| format!("{n} · {pid}"))
    });

    let body = move || match detail.get() {
        None => view! { <Skeleton lines=8 /> }.into_any(),
        Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
        Some(Ok(d)) => detail_body(d, live, gone, cpu_series, mem_series, ask, pid, start_ms, load).into_any(),
    };

    let confirm_view = move || {
        let act = ask.get()?;
        let d = detail.with_untracked(|d| d.as_ref().and_then(|d| d.as_ref().ok()).cloned())?;
        let name = d.info.name.clone();
        let group = group_title(&d.group);
        let (title, text, action, danger) = match act {
            Act::Term => (tf("proc.term_title", &[("name", &name)]), t("proc.term_text").to_string(), "proc.term", true),
            Act::Kill => (tf("proc.kill_title", &[("name", &name)]), t("proc.kill_text").to_string(), "proc.kill", true),
            Act::RestartService => {
                // the engine itself: every container stops and starts again
                let engine = d.group.id.strip_suffix(".service").is_some_and(|u| matches!(u, "docker" | "containerd"));
                let text = t(if engine { "proc.restart_docker_text" } else { "proc.restart_svc_text" }).to_string();
                (tf("proc.restart_svc_title", &[("name", &group)]), text, "proc.restart_svc", engine)
            }
            Act::RestartContainer => (tf("proc.restart_ct_title", &[("name", &group)]), t("proc.restart_ct_text").to_string(), "proc.restart_ct", false),
        };
        let cid = d.group.id.clone();
        let go = Callback::new(move |_| {
            busy.set(true);
            error.set(None);
            let cid = cid.clone();
            spawn_local(async move {
                let done = match act {
                    Act::Term | Act::Kill => {
                        let signal = if act == Act::Term { ProcSignal::Term } else { ProcSignal::Kill };
                        api::post_empty(&format!("{URL}/{pid}/signal"), Some(&SignalRequest { signal, start_ms })).await.map(|_| t("proc.signal_sent").to_string())
                    }
                    Act::RestartService | Act::RestartContainer => {
                        let (url, body) = if act == Act::RestartService {
                            (format!("{URL}/{pid}/restart-service"), serde_json::to_string(&RestartRequest { start_ms }).ok())
                        } else {
                            (format!("/api/v1/docker/containers/{}/restart", api::encode(&cid)), None)
                        };
                        match crate::tasks::run_task(&url, body).await {
                            Ok(task) if task.status == TaskStatus::Ok => Ok(t("proc.restarted").to_string()),
                            Ok(task) => {
                                let _ = error.try_set(Some(crate::tasks::task_error(&task)));
                                let _ = busy.try_set(false);
                                return;
                            }
                            Err(e) => Err(e),
                        }
                    }
                };
                let _ = busy.try_set(false);
                match done {
                    Ok(msg) => {
                        toast(Tone::Ok, msg);
                        let _ = ask.try_set(None);
                    }
                    Err(e) => {
                        let _ = error.try_set(Some(e.text()));
                    }
                }
            });
        });
        let cancel = Callback::new(move |_| {
            error.set(None);
            ask.set(None);
        });
        Some(ui::confirm(Confirm { busy, error, ..Confirm::new(title, text, action, danger, go, cancel) }))
    };

    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    view! {
        {ui::drawer(title, close, view! { <div class="drawer-sec">{body}</div> }.into_any(), foot)}
        {confirm_view}
    }
    .into_any()
}

#[allow(clippy::too_many_arguments)]
fn detail_body(
    d: ProcessDetail,
    live: Memo<Option<(u64, ProcessInfo)>>,
    gone: Memo<bool>,
    cpu_series: Signal<(Vec<f64>, Vec<Vec<f32>>)>,
    mem_series: Signal<(Vec<f64>, Vec<Vec<f32>>)>,
    ask: RwSignal<Option<Act>>,
    pid: u32,
    start_ms: u64,
    reload: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let info = d.info.clone();
    let lv = move |f: fn(&ProcessInfo) -> String| {
        let fallback = f(&info);
        move || live.with(|l| l.as_ref().map(|(_, p)| f(p))).unwrap_or_else(|| fallback.clone())
    };
    let kv = |label: &'static str, value: AnyView| view! { <div class="kv"><dt>{tr(label)}</dt><dd class="tnum">{value}</dd></div> };
    let protected = d.info.protected;
    let g = d.group.clone();
    let restart = (!protected && g.restartable).then(|| {
        let (note, act, label) = match g.kind {
            GroupKind::Container => ("proc.in_container", Act::RestartContainer, "proc.restart_ct"),
            _ => ("proc.in_service", Act::RestartService, "proc.restart_svc"),
        };
        view! {
            {ui::note(Tone::Info, icons::INFO, view! { {tf(note, &[("name", &group_title(&g))])} }.into_any())}
            <div class="sec-actions">
                <button type="button" class="btn primary" on:click=move |_| ask.set(Some(act))>
                    <Icon icon=icons::REFRESH_CW size=16 />{tr(label)}
                </button>
            </div>
        }
    });
    let ports = if d.ports.is_empty() {
        None
    } else {
        Some(d.ports.iter().map(|p| format!("{} {}:{}", p.protocol, p.address, p.port)).collect::<Vec<_>>().join(", "))
    };
    let start_text = i18n::datetime(d.info.start_ms);
    let age = move || {
        let now = js_sys::Date::now() as u64;
        i18n::duration(now.saturating_sub(start_ms) / 1000)
    };
    let opt_bytes = |v: Option<u64>| v.map_or_else(|| "—".to_string(), i18n::bytes);
    let io_text = match d.io_class {
        IoClass::Idle => t("proc.io_idle").to_string(),
        IoClass::Realtime => tf("proc.io_rt", &[("level", &d.io_level.to_string())]),
        IoClass::BestEffort => tf("proc.io_be", &[("level", &d.io_level.to_string())]),
        IoClass::None => t("proc.io_normal").to_string(),
    };
    view! {
        {move || gone.get().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("proc.ended")} }.into_any()))}
        {protected.then(|| ui::note(Tone::Info, icons::SHIELD_CHECK, view! { {tr("proc.protected")} }.into_any()))}
        {restart}
        <div class="form-row">
            <div>
                <h3 class="drawer-h">{tr("proc.cpu_graph")}</h3>
                <div class="chart-wrap">
                    <StillChart colors=&["--c1"] data=cpu_series label=pct_label aria=Signal::derive(move || tf("proc.cpu_aria", &[("v", &live.with(|l| cpu_text(l.as_ref().and_then(|x| x.1.cpu_pct))))])) />
                </div>
            </div>
            <div>
                <h3 class="drawer-h">{tr("proc.mem_graph")}</h3>
                <div class="chart-wrap">
                    <StillChart colors=&["--c2"] data=mem_series binary=true label=bytes_label aria=Signal::derive(move || tf("proc.mem_aria", &[("v", &live.with(|l| l.as_ref().map(|x| i18n::bytes(x.1.rss_bytes)).unwrap_or_default()))])) />
                </div>
            </div>
        </div>
        <dl class="kvs">
            {kv("proc.f.group", view! { {format!("{} · {}", group_title(&g), t(kind_key(g.kind)))} }.into_any())}
            {kv("proc.col.user", view! { {d.info.user.clone()} }.into_any())}
            {kv("col.status", view! { {lv(|p| t(state_key(p.state)).to_string())} }.into_any())}
            {kv("proc.col.cpu", view! { {lv(|p| cpu_text(p.cpu_pct))} }.into_any())}
            {kv("proc.f.rss", view! { {lv(|p| i18n::bytes(p.rss_bytes))} }.into_any())}
            {kv("proc.f.pss", view! { {opt_bytes(d.pss_bytes)} }.into_any())}
            {kv("proc.f.swap", view! { {opt_bytes(d.swap_bytes)} }.into_any())}
            {kv("proc.read", view! { {lv(|p| opt_rate(p.read_bytes_s))} }.into_any())}
            {kv("proc.write", view! { {lv(|p| opt_rate(p.write_bytes_s))} }.into_any())}
            {kv("proc.f.io_total", view! { {format!("{} / {}", opt_bytes(d.read_total_bytes), opt_bytes(d.write_total_bytes))} }.into_any())}
            {kv("proc.f.threads", view! { {lv(|p| p.threads.to_string())} }.into_any())}
            {kv("proc.f.nice", view! { {lv(|p| p.nice.to_string())} }.into_any())}
            {kv("proc.io_prio", view! { {io_text} }.into_any())}
            {kv("proc.f.parent", view! { {d.info.ppid.to_string()} }.into_any())}
            {kv("proc.f.started", view! { {start_text} " · " {age} }.into_any())}
            {ports.map(|p| kv("proc.f.ports", view! { <span class="mono">{p}</span> }.into_any()))}
            {d.exe.clone().map(|e| kv("proc.f.exe", view! { <span class="mono">{e}</span> }.into_any()))}
            {d.cgroup.clone().map(|c| kv("proc.f.cgroup", view! { <span class="mono">{c}</span> }.into_any()))}
        </dl>
        <h3 class="drawer-h">{tr("proc.f.cmdline")}</h3>
        <pre class="proc-cmdline mono">{if d.cmdline.is_empty() { format!("[{}]", d.info.name) } else { d.cmdline.clone() }}</pre>
        {(!protected).then(|| priority_form(&d, pid, start_ms, reload))}
        {(!protected).then(|| view! {
            <h3 class="drawer-h">{tr("proc.stop")}</h3>
            <p class="field-help">{tr("proc.stop_help")}</p>
            <div class="sec-actions">
                <button type="button" class="btn ghost" on:click=move |_| ask.set(Some(Act::Term))><Icon icon=icons::SQUARE size=16 />{tr("proc.term")}</button>
                <button type="button" class="btn danger" on:click=move |_| ask.set(Some(Act::Kill))><Icon icon=icons::CIRCLE_X size=16 />{tr("proc.kill")}</button>
            </div>
        })}
    }
}

fn pct_label(v: f32) -> String {
    i18n::pct(v as f64)
}

fn bytes_label(v: f32) -> String {
    i18n::bytes(v as u64)
}

/// Named nice level and I/O class, applied together.
fn priority_form(d: &ProcessDetail, pid: u32, start_ms: u64, reload: impl Fn() + Copy + Send + Sync + 'static) -> AnyView {
    let nice0 = d.info.nice.to_string();
    let level = RwSignal::new(if LEVELS.iter().any(|(v, _)| *v == nice0) { nice0.clone() } else { "0".to_string() });
    let io0 = if d.io_class == IoClass::Idle { "idle" } else { "best_effort" };
    let io = RwSignal::new(io0.to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let custom = (!LEVELS.iter().any(|(v, _)| *v == nice0)).then(|| tf("proc.prio_custom", &[("nice", &nice0)]));
    let apply = move |_| {
        busy.set(true);
        error.set(None);
        let nice = level.get_untracked().parse::<i32>().ok();
        let io_class = Some(if io.get_untracked() == "idle" { IoClass::Idle } else { IoClass::BestEffort });
        spawn_local(async move {
            let body = PriorityRequest { start_ms, nice, io_class, io_level: 4 };
            match api::post_empty(&format!("{URL}/{pid}/priority"), Some(&body)).await {
                Ok(()) => {
                    toast(Tone::Ok, t("proc.prio_done").to_string());
                    reload();
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    view! {
        <h3 class="drawer-h">{tr("proc.priority")}</h3>
        {custom.map(|c| view! { <p class="field-help">{c}</p> })}
        <div class="form-row">
            {select_field("proc.cpu_prio", None, LEVELS, level.into(), Callback::new(move |v| level.set(v)))}
            {select_field("proc.io_prio", None, IO, io.into(), Callback::new(move |v| io.set(v)))}
        </div>
        {crate::form::form_error(error)}
        <div class="sec-actions">
            <button type="button" class="btn ghost" disabled=move || busy.get() on:click=apply>{spinner(busy)}{tr("proc.prio_apply")}</button>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, group: u32, cpu: f32, mem: u64, name: &str) -> ProcessInfo {
        ProcessInfo { pid, ppid, group, cpu_pct: Some(cpu), rss_bytes: mem, name: name.into(), start_ms: 1000 + pid as u64, ..ProcessInfo::default() }
    }

    fn list() -> ProcessList {
        let g = |kind, id: &str| ProcessGroup { kind, id: id.into(), name: id.into(), ..ProcessGroup::default() };
        ProcessList {
            groups: vec![g(GroupKind::System, "init"), g(GroupKind::Service, "smbd"), g(GroupKind::Container, "abc")],
            processes: vec![p(1, 0, 0, 0.1, 10, "systemd"), p(10, 1, 1, 5.0, 100, "smbd"), p(11, 10, 1, 20.0, 50, "smbd"), p(20, 1, 2, 1.0, 900, "postgres")],
            ..ProcessList::default()
        }
    }

    #[test]
    fn groups_sorted_by_totals() {
        let l = list();
        let r = build(&l, "groups", "cpu", "", &HashSet::new());
        assert_eq!(r.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["gsmbd", "gabc", "ginit"]);
        assert_eq!(r[0].1.cpu, Some(25.0));
        let open: HashSet<String> = ["smbd".to_string()].into_iter().collect();
        let r = build(&l, "groups", "mem", "", &open);
        assert_eq!(r[0].0, "gabc");
        assert_eq!(r.iter().filter(|x| !x.1.group).count(), 2, "the open group's processes");
    }

    #[test]
    fn tree_and_filter() {
        let l = list();
        let r = build(&l, "tree", "cpu", "", &HashSet::new());
        let order: Vec<(u32, u8)> = r.iter().map(|x| (x.1.pid, x.1.depth)).collect();
        assert_eq!(order, [(1, 0), (10, 1), (11, 2), (20, 1)]);
        // a match keeps its ancestors
        let r = build(&l, "tree", "cpu", "postgres", &HashSet::new());
        assert_eq!(r.iter().map(|x| x.1.pid).collect::<Vec<_>>(), [1, 20]);
        let r = build(&l, "list", "mem", "", &HashSet::new());
        assert_eq!(r[0].1.pid, 20);
        let r = build(&l, "groups", "cpu", "smbd", &HashSet::new());
        assert_eq!(r.len(), 3, "a filter opens the matching group");
    }
}
