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

/// Text of a choice: "volume1 (1.2 TB free)", "System (80 GB free)".
pub fn option_text(p: &VolumePlace) -> String {
    let n = name(&p.label);
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
