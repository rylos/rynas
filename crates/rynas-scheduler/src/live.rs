//! Live output of a running task (issue #12), the parts without I/O: the `journalctl`
//! arguments that follow one invocation, how a line of `journalctl -o json` becomes the raw
//! bytes of one output line, and how lines are batched into the data of a server-sent event.
//!
//! journald splits a service's standard output at newlines: a carriage return (`rsync -P`,
//! `curl`) stays inside the entry, and a progress line reaches the journal when its line ends
//! (or at journald's `LineMax=`, 48 KiB by default). The browser keeps what follows the last
//! carriage return of each entry.

use serde::Deserialize;

/// Entries read from before the stream opened: a run that already printed more shows its
/// last lines first, then follows.
pub const BACKLOG_LINES: usize = 5000;
/// Longest line accepted from `journalctl -o json` (an entry is at most `LineMax=`, 48 KiB by
/// default, about 4 bytes per byte as a JSON array of numbers); longer lines are skipped.
pub const MAX_ENTRY_JSON: usize = 1 << 20;
/// A `lines` event is sent once its entries reach this many bytes …
pub const BATCH_BYTES: usize = 64 * 1024;
/// … or this many entries, whichever comes first (and at least every 150 ms otherwise).
pub const BATCH_LINES: usize = 500;

/// A systemd invocation id: 32 hex digits (accepted up to 64, like the history).
pub fn valid_invocation_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Arguments of `journalctl` that print the entries of one invocation (the last
/// [`BACKLOG_LINES`], then every new one) as JSON, one per line, only the message. `--all`:
/// without it messages over 4096 bytes come out as `null`.
pub fn follow_args(invocation_id: &str) -> Result<Vec<String>, String> {
    if !valid_invocation_id(invocation_id) {
        return Err("invalid invocation id".into());
    }
    Ok(vec![
        "--no-pager".into(),
        "--quiet".into(),
        "--all".into(),
        "--output=json".into(),
        "--output-fields=MESSAGE".into(),
        "--follow".into(),
        format!("--lines={BACKLOG_LINES}"),
        format!("_SYSTEMD_INVOCATION_ID={invocation_id}"),
    ])
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "MESSAGE", default)]
    message: Option<Message>,
}

/// journalctl prints a field as a string when it is printable UTF-8, otherwise (a carriage
/// return, an escape sequence, bytes that are not UTF-8) as an array of byte values.
#[derive(Deserialize)]
#[serde(untagged)]
enum Message {
    Text(String),
    Bytes(Vec<u8>),
}

/// The raw bytes of the message of one `journalctl -o json` line; `None` for a line that is
/// not an entry or has no message.
pub fn entry_message(line: &[u8]) -> Option<Vec<u8>> {
    let e: Entry = serde_json::from_slice(line).ok()?;
    match e.message? {
        Message::Text(s) => Some(s.into_bytes()),
        Message::Bytes(b) => Some(b),
    }
}

/// Data of a `lines` event: a JSON array of strings, one per entry. Bytes that are not UTF-8
/// become U+FFFD; JSON escapes carriage returns, newlines and every other control
/// character, so the data is one line of text whatever the output holds (a raw CR or LF
/// would end the SSE field early).
pub fn lines_data<L: AsRef<[u8]>>(lines: &[L]) -> String {
    let texts: Vec<_> = lines
        .iter()
        .map(|l| String::from_utf8_lossy(l.as_ref()))
        .collect();
    serde_json::to_string(&texts).unwrap_or_else(|_| "[]".into())
}

/// Entries waiting to be sent together.
#[derive(Debug, Default)]
pub struct Batch {
    lines: Vec<Vec<u8>>,
    bytes: usize,
}

impl Batch {
    /// Adds an entry; `true` when the batch is full and should be sent now.
    pub fn push(&mut self, line: Vec<u8>) -> bool {
        self.bytes += line.len();
        self.lines.push(line);
        self.bytes >= BATCH_BYTES || self.lines.len() >= BATCH_LINES
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The event data of what is waiting ([`lines_data`]), emptying the batch.
    pub fn take(&mut self) -> String {
        self.bytes = 0;
        lines_data(&std::mem::take(&mut self.lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_only_valid_invocations() {
        let a = follow_args("491bde0fd72e4dfda8e2b8739f61bce5").unwrap();
        assert!(a.contains(&"--follow".to_string()));
        assert!(a.contains(&"--all".to_string()));
        assert_eq!(a.last().unwrap(), "_SYSTEMD_INVOCATION_ID=491bde0fd72e4dfda8e2b8739f61bce5");
        for bad in ["", "abc def", "../x", "_SYSTEMD_UNIT=x", &"a".repeat(65)] {
            assert!(follow_args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reads_text_and_byte_messages() {
        // real lines of journalctl -o json --output-fields=MESSAGE (cursor fields cut)
        let text = br#"{"__CURSOR":"s=1;i=2","MESSAGE":"sending incremental file list","_BOOT_ID":"x"}"#;
        assert_eq!(entry_message(text).unwrap(), b"sending incremental file list");
        // "a 10%\rb 50%" and a byte that is not UTF-8
        let cr = br#"{"MESSAGE":[97,32,49,48,37,13,98,32,53,48,37]}"#;
        assert_eq!(entry_message(cr).unwrap(), b"a 10%\rb 50%");
        let bad = br#"{"MESSAGE":[98,97,100,32,255]}"#;
        assert_eq!(entry_message(bad).unwrap(), b"bad \xff");
        assert_eq!(entry_message(br#"{"MESSAGE":null}"#), None);
        assert_eq!(entry_message(br#"{"__CURSOR":"x"}"#), None);
        assert_eq!(entry_message(b"-- No entries --"), None);
    }

    #[test]
    fn event_data_is_one_line_and_keeps_the_text() {
        let lines: Vec<&[u8]> = vec![b"a 10%\rb 50%", b"two\nlines", b"esc \x1b[1m bold", b"bad \xff byte", b""];
        let data = lines_data(&lines);
        assert!(!data.contains('\r') && !data.contains('\n'), "{data}");
        let back: Vec<String> = serde_json::from_str(&data).unwrap();
        assert_eq!(back[0], "a 10%\rb 50%");
        assert_eq!(back[1], "two\nlines");
        assert_eq!(back[2], "esc \u{1b}[1m bold");
        assert_eq!(back[3], "bad \u{fffd} byte");
        assert_eq!(back[4], "");
        assert_eq!(lines_data::<&[u8]>(&[]), "[]");
    }

    #[test]
    fn batches_by_bytes_or_lines() {
        let mut b = Batch::default();
        assert!(b.is_empty());
        for i in 0..BATCH_LINES - 1 {
            assert!(!b.push(format!("line {i}").into_bytes()));
        }
        assert!(b.push(b"last".to_vec()));
        let data = b.take();
        assert!(b.is_empty());
        assert_eq!(serde_json::from_str::<Vec<String>>(&data).unwrap().len(), BATCH_LINES);
        assert!(b.push(vec![b'x'; BATCH_BYTES]));
    }
}
