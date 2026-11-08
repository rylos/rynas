//! The volumes something can be put on, the system volume included (D-ROOTVOL): their names,
//! the text of a choice and the note shown next to a chosen place (`GET
//! /api/v1/storage/places`, or the volume list of the Docker settings).

use leptos::prelude::*;
use rynas_api_types::storage::{SYSTEM_VOLUME, VolumePlace};

use crate::i18n::{self, t, tf};
use crate::icons;
use crate::ui::{self, Tone};

/// `GET` path of the places.
pub const PATH: &str = "/api/v1/storage/places";

/// Shown name of a volume label: "System" for the system volume.
pub fn name(label: &str) -> String {
    if label == SYSTEM_VOLUME {
        t("place.system").to_string()
    } else {
        label.to_string()
    }
}

pub fn option_text(p: &VolumePlace) -> String {
    let n = name(&p.label);
    if p.unavailable.is_some() {
        return format!("{n} ({})", t("place.unavailable_short"));
    }
    match p.avail_bytes {
        Some(a) => format!("{n} ({})", tf("dk.vol_free", &[("free", &i18n::bytes(a))])),
        None => n,
    }
}

/// The note next to a chosen place: little free space, or what the system volume means
/// (its folder, the space shared with the operating system); `btrfs_note` adds that a
/// non-btrfs system disk gets a plain folder (no subvolume, snapshots or NOCOW).
pub fn note(p: Option<&VolumePlace>, btrfs_note: bool) -> Option<AnyView> {
    let p = p?;
    if let Some(why) = p.unavailable.as_deref() {
        let text = tf("place.unavailable", &[("name", &name(&p.label)), ("why", why)]);
        return Some(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(text)));
    }
    let low = p.low_space.then(|| {
        let free = i18n::bytes(p.avail_bytes.unwrap_or(0));
        ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(tf("place.low_space", &[("name", &name(&p.label)), ("free", &free)])))
    });
    if !p.system {
        return low;
    }
    let mut text = tf("place.system_note", &[("path", &p.path)]);
    if btrfs_note && !p.btrfs() {
        text = format!("{text} {}", t("place.not_btrfs"));
    }
    Some(view! { {ui::note(Tone::Info, icons::INFO, ui::text(text))}{low} }.into_any())
}

/// Why the places that cannot be used now are not offered (their reason from rynasd), one
/// warning each; nothing when all can be used.
pub fn unavailable_notes(places: &[VolumePlace]) -> Option<AnyView> {
    let v: Vec<AnyView> = places
        .iter()
        .filter_map(|p| {
            let why = p.unavailable.as_deref()?;
            let text = tf("place.unavailable", &[("name", &name(&p.label)), ("why", why)]);
            Some(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(text)))
        })
        .collect();
    (!v.is_empty()).then(|| v.into_any())
}

/// The places as a segmented choice (radio group, wraps on a narrow screen): `on` tells
/// the chosen one, `pick` runs on a click; a place that cannot be used now is shown
/// disabled, with [`unavailable_notes`] below.
pub fn picker(label: &'static str, places: Vec<VolumePlace>, on: Callback<VolumePlace, bool>, pick: Callback<VolumePlace>) -> AnyView {
    let id = crate::form::fid();
    let notes = unavailable_notes(&places);
    view! {
        <div class="place-picker">
            <span class="field-label" id=id.clone()>{crate::i18n::tr(label)}</span>
            <div class="segmented wrap" role="radiogroup" aria-labelledby=id>
                {places.into_iter().map(|p| {
                    let (p1, p2) = (p.clone(), p.clone());
                    let (off, why, text) = (p.unavailable.is_some(), p.unavailable.clone(), name(&p.label));
                    view! {
                        <button
                            type="button"
                            role="radio"
                            aria-checked=move || on.run(p1.clone()).to_string()
                            class:on=move || on.run(p2.clone())
                            disabled=off
                            title=why
                            on:click=move |_| pick.run(p.clone())
                        >{text}</button>
                    }
                }).collect_view()}
            </div>
        </div>
        {notes}
    }
    .into_any()
}
