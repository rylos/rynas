//! "Ask for approval": the restore TUI's login without a code typed in
//! (`docs/phase2/pc-restore-approval.md`). The TUI commits to a fresh secret, asks the NAS
//! for an approval with the PC as the person names it, reveals the secret once it has the
//! NAS' nonce, and shows the compare code (made here from secret, nonce and the certificate
//! this TUI really sees) and a QR code for the app. The administrator types that code on the
//! phone or in the web interface to approve; the answer carries a restore code used exactly
//! like a typed one.
//!
//! The QR code is drawn with half blocks (`▀ ▄ █`, in the CP437 range of the Linux console's
//! own font and of Terminus, the fonts of live systems such as the Arch ISO) in white on
//! black whatever the terminal's colours, with a 2-module quiet zone. The link is at most
//! 78 bytes (version 4 at the lowest error correction: 33 modules, 19 text rows with the quiet
//! zone, 37 columns), so it fits next to the text on an 80×25 console. A terminal that is too
//! small or not UTF-8 gets the text code only (the phone's push notification needs no QR).

use std::io;
use std::sync::mpsc;
use std::time::Duration;

use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};
use rynas_api_types::pcagent::{
    AGENT_RESTORE_REQUESTS, RestoreApproval, RestoreQr, RestoreRequestCreate, RestoreRequestCreated, RestoreRequestPoll,
    RestoreRequestReveal, RestoreRequestState, restore_commit_input, restore_compare_code, restore_compare_input,
};
use sha2::{Digest, Sha256};

use crate::http::{Client, Endpoint};

/// The quiet zone around the QR code, in modules.
pub const QUIET: usize = 2;
/// The largest QR version drawn (33 modules).
pub const MAX_VERSION: u8 = 4;
/// Seconds of one long poll (the NAS waits at most 25).
const POLL_WAIT_S: u64 = 20;

/// A request the NAS took.
#[derive(Debug, Clone)]
pub struct Asked {
    pub created: RestoreRequestCreated,
    /// The compare code made here (six digits).
    pub code: String,
    /// The QR code's link.
    pub link: String,
}

/// The compare code of this TUI's `secret`, the NAS' `nonce` and the certificate this TUI
/// saw (`fp`, empty over plain HTTP).
pub fn compare_code(secret: &str, nonce: &str, fp: &str) -> String {
    restore_compare_code(&Sha256::digest(restore_compare_input(secret, nonce, fp).as_bytes()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The commitment to `secret` sent before the NAS' nonce is known.
pub fn commitment(secret: &str) -> String {
    hex(&Sha256::digest(restore_commit_input(secret).as_bytes()))
}

/// 256 random bits, hex (the kernel's generator).
fn new_secret() -> Result<String, String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("random numbers: {e}"))?;
    Ok(hex(&b))
}

/// Asks the NAS at `client` (pinned to `fp`) for an approval of the restore of `pc`: the
/// commitment to a fresh secret first, the NAS' nonce back, then the secret revealed (only
/// then do administrators see the request). The code is made here from what this TUI saw.
pub fn ask(client: &Client, pc: &str, fp: &str) -> Result<Asked, String> {
    let secret = new_secret()?;
    let created: RestoreRequestCreated = client.post_json(
        AGENT_RESTORE_REQUESTS,
        &RestoreRequestCreate {
            pc: pc.trim().to_string(),
            fingerprint: fp.to_string(),
            commit: commitment(&secret),
        },
    )?;
    let token_client = Client::new(client.ep(), fp).with_token(&created.poll_token);
    let () = token_client.post_json(&format!("{}/reveal", path_of(&created.id)), &RestoreRequestReveal { secret: secret.clone() })?;
    let code = compare_code(&secret, &created.nonce, fp);
    let link = RestoreQr {
        nas: created.sid.clone(),
        req: created.id.clone(),
        code: code.clone(),
    }
    .link();
    Ok(Asked { created, code, link })
}

/// What the waiting screen hears from the poller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    Approved(RestoreApproval),
    /// Denied, expired or withdrawn.
    Ended(RestoreRequestState),
    /// The NAS no longer knows the request (rynasd restarted, or the request ended long ago).
    Unknown,
    /// The NAS cannot be reached right now; the poller tries again.
    Trouble(String),
}

fn path_of(id: &str) -> String {
    format!("{AGENT_RESTORE_REQUESTS}/{id}")
}

/// One poll: the state, waiting up to `wait` seconds on the NAS.
pub fn poll_once(client: &Client, id: &str, wait: u64) -> io::Result<Result<RestoreRequestPoll, (u16, String)>> {
    let r = client.request("GET", &format!("{}?wait={wait}", path_of(id)), &[("Accept", "application/json")], &[])?;
    if !r.ok() {
        return Ok(Err((r.status, r.error_text())));
    }
    Ok(serde_json::from_slice(&r.body).map_err(|e| (0, format!("bad answer from the NAS: {e}"))))
}

/// The poller: long polls in a thread of its own until the request ends, telling `tx` what
/// it hears; network trouble is retried with a growing pause (1 s … 15 s). It stops when the
/// receiver is gone (the screen was left).
pub fn spawn_poller(ep: Endpoint, fp: String, id: String, token: String, tx: mpsc::Sender<Heard>) {
    std::thread::spawn(move || {
        let client = Client::new(ep, &fp).with_token(&token);
        let mut pause = 1;
        loop {
            let heard = match poll_once(&client, &id, POLL_WAIT_S) {
                Ok(Ok(p)) => match (p.state, p.approval) {
                    (RestoreRequestState::Pending, _) => None,
                    (RestoreRequestState::Approved, Some(a)) => Some(Heard::Approved(a)),
                    (RestoreRequestState::Approved, None) => Some(Heard::Trouble("approved, but without a code: ask again".into())),
                    (s, _) => Some(Heard::Ended(s)),
                },
                Ok(Err((404, _))) => Some(Heard::Unknown),
                Ok(Err((_, e))) => Some(Heard::Trouble(e)),
                Err(e) => Some(Heard::Trouble(e.to_string())),
            };
            let done = matches!(heard, Some(Heard::Approved(_) | Heard::Ended(_) | Heard::Unknown));
            let trouble = matches!(heard, Some(Heard::Trouble(_)));
            // a screen left meanwhile: nobody listens (a withdrawn request ends the next poll)
            if let Some(h) = heard
                && tx.send(h).is_err()
            {
                return;
            }
            if done {
                return;
            }
            if trouble {
                std::thread::sleep(Duration::from_secs(pause));
                pause = (pause * 2).min(15);
            } else {
                pause = 1;
            }
        }
    });
}

/// Withdraws the request (Esc): its approval can no longer be used. Errors are ignored (the
/// request ends by itself after 10 minutes).
pub fn withdraw(ep: Endpoint, fp: &str, id: &str, token: &str) {
    let client = Client::new(ep, fp).with_token(token);
    let _ = client.request("DELETE", &path_of(id), &[], &[]);
}

/// The QR code of `link` as rows of half blocks, light modules drawn (white on black);
/// `None` when the link needs more than [`MAX_VERSION`].
pub fn qr_rows(link: &str) -> Option<Vec<String>> {
    let segs = QrSegment::make_segments(link);
    let qr = QrCode::encode_segments_advanced(&segs, QrCodeEcc::Low, Version::MIN, Version::new(MAX_VERSION), None, true).ok()?;
    let n = qr.size() as usize;
    let side = n + 2 * QUIET;
    // light = true; the quiet zone is light
    let light = |x: usize, y: usize| -> bool {
        if x < QUIET || y < QUIET || x >= QUIET + n || y >= QUIET + n {
            return true;
        }
        !qr.get_module((x - QUIET) as i32, (y - QUIET) as i32)
    };
    let mut rows = Vec::with_capacity(side.div_ceil(2));
    for y in (0..side).step_by(2) {
        let mut s = String::with_capacity(side * 3);
        for x in 0..side {
            let top = light(x, y);
            // below the last row: the background (black), so the half shows the edge
            let bottom = y + 1 < side && light(x, y + 1);
            s.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        rows.push(s);
    }
    Some(rows)
}

/// Columns the text next to the QR code needs at least.
pub const TEXT_MIN_COLS: u16 = 34;

/// Whether `rows` of a QR code fit a terminal of `cols`×`lines` next to the text (one column
/// of borders on each side, two lines of borders and a line of keys).
pub fn qr_fits(rows: &[String], cols: u16, lines: u16) -> bool {
    let w = rows.first().map(|r| r.chars().count()).unwrap_or(0) as u16;
    let h = rows.len() as u16;
    cols >= w + 2 + TEXT_MIN_COLS && lines >= h + 3
}

/// The console takes UTF-8: a UTF-8 locale, or the Linux console in its default UTF-8 mode
/// (`vt.default_utf8`) without any locale set, as on a live system's text console.
pub fn utf8_terminal() -> bool {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    match locale {
        Some(l) => {
            let l = l.to_ascii_lowercase();
            l.contains("utf-8") || l.contains("utf8")
        }
        None => {
            std::env::var("TERM").is_ok_and(|t| t == "linux")
                && std::fs::read_to_string("/sys/module/vt/parameters/default_utf8").is_ok_and(|v| v.trim() == "1")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The longest link the NAS makes: a 22-character sid, 16 hex digits, 6 digits.
    fn longest() -> String {
        RestoreQr {
            nas: "AbCdEfGhIjKlMnOpQrSt_-".into(),
            req: "0123456789abcdef".into(),
            code: "987654".into(),
        }
        .link()
    }

    #[test]
    fn the_qr_code_fits_an_80_by_25_console() {
        let link = longest();
        assert!(link.len() <= 78, "{} bytes", link.len());
        let rows = qr_rows(&link).expect("version 4 at most");
        // version 4: 33 modules + 2×2 quiet = 37 columns, 19 rows of half blocks
        assert!(rows.len() <= 19, "{} rows", rows.len());
        assert!(rows.iter().all(|r| r.chars().count() == rows[0].chars().count()));
        assert!(rows[0].chars().count() <= 37);
        assert!(qr_fits(&rows, 80, 25));
        // a smaller terminal shows the text code only
        assert!(!qr_fits(&rows, 80, 20));
        assert!(!qr_fits(&rows, 60, 25));
        // the quiet zone is light all round: the first row is full blocks
        assert!(rows[0].chars().all(|c| c == '█'));
        assert!(rows.iter().all(|r| (r.starts_with("██") && r.ends_with("██")) || r.chars().all(|c| c == '▀' || c == '█')));
        // only the four glyphs of the CP437 range
        assert!(rows.iter().flat_map(|r| r.chars()).all(|c| matches!(c, '█' | '▀' | '▄' | ' ')));
    }

    #[test]
    fn a_link_too_long_has_no_qr_code() {
        assert!(qr_rows(&"x".repeat(200)).is_none());
    }

    #[test]
    fn the_compare_code_matches_the_nas_rule() {
        let fp = "AA:BB:CC";
        let c = compare_code("5e", "00ff", fp);
        assert_eq!(c, restore_compare_code(&Sha256::digest(b"rynas-restore-v2\n5e\n00ff\nAABBCC")));
        assert_eq!(compare_code("5e", "00ff", "aabbcc"), c);
        assert_eq!(c.len(), 6);
        assert_eq!(commitment("5e"), hex(&Sha256::digest(b"rynas-restore-commit-v1\n5e")));
        let s = new_secret().unwrap();
        assert_eq!(s.len(), 64);
        assert_ne!(s, new_secret().unwrap());
    }

    #[test]
    fn the_qr_code_reads_back() {
        // the link the TUI draws is the one the app parses
        let link = longest();
        assert!(RestoreQr::parse(&link).is_some());
    }
}
