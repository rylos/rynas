//! "Android app": where to get the phone app (`docs/phase2/mobile-devices.md` §6). One box,
//! used on the Devices page, in "Pair a device" and in a dialog from the user menu: a QR code
//! of the APK's fixed address, a plain link to it (works when the NAS cannot read the
//! metadata), and, when it could, the version, the size and the signing certificate.

use leptos::prelude::*;
use rynas_api_types::devices::{ANDROID_APK_URL, AndroidApp};

use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::qr::QrSvg;
use crate::ui::{Modal, Remote, WidgetHead, close_foot};

const URL: &str = "/api/v1/devices/android-app";

/// A SHA-256 in hex as Android prints certificate fingerprints: `AB:CD:…`.
fn fingerprint(hex: &str) -> String {
    hex.as_bytes()
        .chunks(2)
        .map(|p| String::from_utf8_lossy(p).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

/// The QR code, the download link and what the NAS knows of the published app.
#[component]
pub fn AndroidAppBox() -> impl IntoView {
    let app = Remote::<AndroidApp>::new(URL);
    let release = move || match app.data.get() {
        Some(AndroidApp::Available(r)) => Some(r),
        _ => None,
    };
    view! {
        <div class="apk">
            <QrSvg text=ANDROID_APK_URL label="dev.app_qr" />
            <div class="apk-body">
                <strong>{tr("dev.app_scan")}</strong>
                {move || release().map(|r| view! {
                    <p class="muted small tnum">{tf("about.version", &[("version", &r.version)])}" · "{i18n::bytes_si(r.size)}</p>
                })}
                <a class="btn" href=ANDROID_APK_URL rel="noopener noreferrer">
                    <Icon icon=icons::DOWNLOAD size=16 /><span class="btn-label">{tr("dev.app_download")}</span>
                </a>
                {move || release().map(|r| view! {
                    <details class="fold sub">
                        <summary><Icon icon=icons::CHEVRON_RIGHT size=14 />{tr("dev.app_signer")}</summary>
                        <code class="mono small apk-fp">{fingerprint(&r.signer_sha256)}</code>
                    </details>
                })}
            </div>
        </div>
    }
}

/// The box as a card of the Devices page.
#[component]
pub fn AndroidAppCard() -> impl IntoView {
    view! {
        <section class="card widget" style="--i:0">
            <WidgetHead icon=icons::SMARTPHONE title="dev.app" />
            <AndroidAppBox />
        </section>
    }
}

/// The box in a dialog (the user menu).
#[component]
pub fn AndroidAppDialog(on_close: Callback<()>) -> impl IntoView {
    view! {
        <Modal title=Signal::derive(|| t("dev.app").to_string()) on_close=on_close foot=move || close_foot(on_close)>
            <AndroidAppBox />
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fingerprints_are_colon_separated_pairs() {
        assert_eq!(super::fingerprint("0a1bff"), "0A:1B:FF");
        assert_eq!(super::fingerprint(""), "");
    }
}
