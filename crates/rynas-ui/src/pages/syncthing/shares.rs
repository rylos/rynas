//! The shared folders the Syncthing app mounts (its share set, each at `/shares/<name>`):
//! listed on the page, one added from the applied shares or one removed, after the plan of
//! the change (`POST /api/v1/apps/installed/{stack}/share-set/plan`: the share permission
//! entries it adds, the items it removes), then the task (`PUT .../share-set`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::apps::{InstalledShareSetItem, ShareSetChoice, ShareSetPlan, ShareSetRequest};
use rynas_api_types::{ShareView, TaskStarted, TaskStatus};

use crate::api;
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::{self, tasks};
use crate::toast::toast;
use crate::ui::{self, Confirm, Remote, Tone, WidgetHead};

fn set_url(stack: &str, plan: bool) -> String {
    format!("/api/v1/apps/installed/{}/share-set{}", api::encode(stack), if plan { "/plan" } else { "" })
}

/// A change asked: the share added or removed, and the whole new list.
#[derive(Clone, Debug, PartialEq)]
struct Change {
    share: String,
    add: bool,
    items: Vec<String>,
}

/// The card; `items` is the share set now, `changed` runs after a change (the page reads
/// the app again).
pub(super) fn card(stack: String, items: Vec<InstalledShareSetItem>, changed: Callback<()>) -> AnyView {
    let shares = Remote::<Vec<ShareView>>::new("/api/v1/shares");
    let pick = RwSignal::new(String::new());
    let ask = RwSignal::new(None::<Change>);
    let names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
    let current = StoredValue::new(names.clone());
    let list = if items.is_empty() {
        ui::empty_line("stg.ss.none")
    } else {
        let rows = items
            .iter()
            .map(|i| {
                let name = i.name.clone();
                let n2 = name.clone();
                let remove = move |_| {
                    let items = current.get_value().into_iter().filter(|x| *x != n2).collect();
                    ask.set(Some(Change { share: n2.clone(), add: false, items }));
                };
                view! {
                    <li class="stg-dev">
                        <span>
                            <strong>{name.clone()}</strong>" "
                            <span class="muted mono small">{format!("/shares/{name}")}</span>
                            {i.follow.then(|| view! { " "{ui::chip(Tone::Info, ui::text(t("stg.ss.follows").to_string()))} })}
                            <span class="cell-sub">{ui::breakable(&i.path, "mono")}</span>
                        </span>
                        <button type="button" class="btn ghost sm" aria-label=ui::verb_name("stg.ss.remove", &name) on:click=remove>
                            <Icon icon=icons::TRASH_2 size=14 />{tr("stg.ss.remove")}
                        </button>
                    </li>
                }
            })
            .collect_view();
        view! { <ul class="plain-list stg-lines">{rows}</ul> }.into_any()
    };
    let choices = move || {
        let cur = current.get_value();
        shares.data.with(|l| {
            l.as_ref()
                .map(|l| l.iter().filter(|s| !s.share.path.is_empty() && !cur.contains(&s.share.name)).map(|s| s.share.name.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    };
    let add = move |_| {
        let share = pick.get_untracked();
        if share.is_empty() {
            return;
        }
        let mut items = current.get_value();
        items.push(share.clone());
        ask.set(Some(Change { share, add: true, items }));
    };
    let fid = crate::form::fid();
    let stack2 = stack.clone();
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::FOLDER title="stg.ss.title" />
            <p class="small muted">{tr("stg.ss.text")}</p>
            {list}
            <div class="field">
                <label class="field-label" for=fid.clone()>{tr("stg.ss.add_label")}</label>
                <div class="stg-volume">
                    <select id=fid class="input" on:change=move |ev| pick.set(event_target_value(&ev))>
                        <option value="" selected=move || pick.with(String::is_empty)>{tr("snap.f.pick_share")}</option>
                        {move || choices().into_iter().map(|n| { let v = n.clone(); view! { <option value=v.clone() selected=move || pick.with(|p| *p == v)>{n}</option> } }).collect_view()}
                    </select>
                    <button type="button" class="btn ghost sm" disabled=move || pick.with(String::is_empty) on:click=add><Icon icon=icons::PLUS size=14 />{tr("stg.ss.add")}</button>
                </div>
            </div>
            {move || ask.get().map(|c| confirm_change(stack2.clone(), c, ask, pick, changed))}
        </section>
    }
    .into_any()
}

/// The confirmation of a change, with its plan.
fn confirm_change(stack: String, c: Change, ask: RwSignal<Option<Change>>, pick: RwSignal<String>, changed: Callback<()>) -> AnyView {
    let plan = RwSignal::new(None::<Result<ShareSetPlan, String>>);
    let req = ShareSetRequest { items: c.items.iter().map(|s| ShareSetChoice { share: s.clone(), follow: None }).collect() };
    let (url, r2) = (set_url(&stack, true), req.clone());
    spawn_local(async move {
        let r = api::post::<_, ShareSetPlan>(&url, Some(&r2)).await.map_err(|e| e.text());
        let _ = plan.try_set(Some(r));
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let cancel = Callback::new(move |_| ask.set(None));
    let put_url = set_url(&stack, false);
    let go = Callback::new(move |_| {
        if busy.get_untracked() || !plan.with_untracked(|p| matches!(p, Some(Ok(_)))) {
            return;
        }
        busy.set(true);
        error.set(None);
        let (url, req) = (put_url.clone(), req.clone());
        spawn_local(async move {
            match api::put::<_, TaskStarted>(&url, &req).await {
                Ok(ts) => {
                    let _ = busy.try_set(false);
                    ask.set(None);
                    pick.set(String::new());
                    tasks().show(&ts.task_id);
                    match tasks::follow(&ts.task_id).await {
                        Ok(tk) if tk.status == TaskStatus::Ok => toast(Tone::Ok, t("stg.ss.done").to_string()),
                        Ok(tk) => toast(Tone::Err, tasks::task_error(&tk)),
                        Err(e) if e.status() == 401 => {}
                        Err(e) => toast(Tone::Err, e.text()),
                    }
                    changed.try_run(());
                }
                Err(e) => {
                    let _ = busy.try_set(false);
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    let extra = move || match plan.get() {
        None => ui::loading(Signal::stored(None), Callback::new(|_| {}), 2),
        Some(Err(e)) => ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::text(e)),
        Some(Ok(p)) => view! {
            {(!p.removed.is_empty()).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(tf("stg.ss.removed", &[("list", &p.removed.join(", "))]))))}
            {super::grants_view(&p.grants)}
        }
        .into_any(),
    };
    let (title, text, danger) = if c.add {
        (tf("stg.ss.add_title", &[("share", &c.share)]), tf("stg.ss.add_text", &[("share", &c.share)]), false)
    } else {
        (tf("stg.ss.remove_title", &[("share", &c.share)]), tf("stg.ss.remove_text", &[("share", &c.share)]), true)
    };
    ui::confirm(Confirm { extra: Some(view! { {extra} }.into_any()), busy, error, ..Confirm::new(title, text, "stg.ss.apply", danger, go, cancel) })
}
