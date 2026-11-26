//! QR code drawn as SVG (qrcodegen, no image or canvas): used for the TOTP enrollment URI, the pairing codes
//! of mobile devices and the Android app's address.

use leptos::prelude::*;
use qrcodegen::{QrCode, QrCodeEcc};

/// Path data: one 1-module-high rectangle per horizontal run of dark modules (4-module quiet zone).
fn path(qr: &QrCode) -> String {
    let n = qr.size();
    let mut d = String::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        let mut x = 0;
        while x < n {
            if qr.get_module(x, y) {
                let start = x;
                while x < n && qr.get_module(x, y) {
                    x += 1;
                }
                d.push_str(&format!("M{},{}h{}v1h-{}z", start + 4, y + 4, x - start, x - start));
            } else {
                x += 1;
            }
        }
    }
    d
}

/// Always dark on white (scanners need it, in the dark theme too). `label`: the i18n key of
/// what the code carries (default: the TOTP secret).
#[component]
pub fn QrSvg(#[prop(into)] text: String, #[prop(optional)] label: Option<&'static str>) -> impl IntoView {
    QrCode::encode_text(&text, QrCodeEcc::Medium).ok().map(|qr| {
        let size = qr.size() + 8;
        view! {
            <svg class="qr" viewBox=format!("0 0 {size} {size}") role="img" aria-label=crate::i18n::tr(label.unwrap_or("sec.qr_label")) shape-rendering="crispEdges">
                <rect width=size height=size class="qr-bg" />
                <path d=path(&qr) class="qr-fg" />
            </svg>
        }
    })
}
