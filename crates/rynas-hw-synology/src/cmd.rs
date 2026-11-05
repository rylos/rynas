//! Microcontroller commands through synohwd's FIFO (`/run/synohwd/cmd`, `synohwd send`).
//!
//! Only what synohwd itself accepts goes out: a sequence of `V<nn>` (exactly two digits) or
//! single commands of `2-9 : ; D R t u q p s r l k @ A B` (`UP_CMD` in synohwd.py). `1` (hard
//! power off) and `C` (reboot) are never sent: the kernel sends them at the end of a
//! poweroff or reboot. synohwd validates again before writing to the UART.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rynas_hw::ControlError;

pub const FIFO: &str = "/run/synohwd/cmd";

/// Short beep.
pub const BEEP_SHORT: &str = "2";
/// Long beep.
pub const BEEP_LONG: &str = "3";

/// Single-character commands of `UP_CMD`.
const SINGLE: &[u8] = b"23456789:;DRtuqpsrlk@AB";

/// Longest command line rynas writes (synohwd reads 256 bytes at a time).
pub const MAX_CMD: usize = 64;

/// synohwd's `check_cmd`: the whole string must be a sequence of known commands.
pub fn check_cmd(cmd: &str) -> Result<(), String> {
    if cmd.is_empty() {
        return Err("empty command".into());
    }
    if cmd.len() > MAX_CMD {
        return Err(format!("command longer than {MAX_CMD} characters"));
    }
    let b = cmd.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'V'
            && b.len() >= i + 3
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
        {
            i += 3;
        } else if SINGLE.contains(&b[i]) {
            i += 1;
        } else {
            // synohwd's message for the hard power-off
            if without_duties(cmd).contains('1') {
                return Err(format!("{cmd:?}: '1' is the uP hard power-off, refused"));
            }
            return Err(format!(
                "{cmd:?}: not a sequence of uP commands (V<nn> or one of 2-9:;DRtuqpsrlk@AB)"
            ));
        }
    }
    Ok(())
}

/// `re.sub(r"V\d\d", "", cmd)`.
fn without_duties(cmd: &str) -> String {
    let b = cmd.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'V'
            && b.len() >= i + 3
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
        {
            i += 3;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// Writes one command line to the FIFO like `synohwd send` does: non-blocking open (no reader
/// = synohwd is not running), one write.
pub fn send(fifo: &Path, cmd: &str) -> Result<(), ControlError> {
    check_cmd(cmd).map_err(ControlError::Invalid)?;
    let down = |why: &str| ControlError::Unavailable(format!("synohwd is not running ({why})"));
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(fifo)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(down("no command FIFO"));
        }
        Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
            return Err(down("nobody reads the command FIFO"));
        }
        Err(e) => return Err(ControlError::Failed(format!("synohwd command FIFO: {e}"))),
    };
    use std::os::unix::fs::FileTypeExt;
    if !f
        .metadata()
        .map(|m| m.file_type().is_fifo())
        .unwrap_or(false)
    {
        return Err(ControlError::Failed(
            "the synohwd command path is not a FIFO".into(),
        ));
    }
    match f.write_all(format!("{cmd}\n").as_bytes()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(ControlError::Busy(
            "synohwd is busy (command FIFO full), try again".into(),
        )),
        Err(e) => Err(ControlError::Failed(format!("synohwd command FIFO: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The cases of synohwd's own test (`test_synohwd.py`: "'1' blocking", "malformed uP
    /// commands", accepted commands).
    #[test]
    fn parity_with_synohwd_check_cmd() {
        for (cmd, ok) in [
            ("V10", true),
            ("V99", true),
            ("V01", true),
            ("V10V11", true),
            ("2", true),
            ("1", false),
            ("21", false),
            ("V1", false),
            ("V101", false),
            ("LA1", false),
            ("1V10", false),
            ("", false),
        ] {
            assert_eq!(check_cmd(cmd).is_ok(), ok, "{cmd:?}");
        }
        for cmd in [
            "VV11", "V", "V9", "VX1", "C", "2C", "LA1", "x", "V10 ", "V1a", "VVV",
        ] {
            assert!(check_cmd(cmd).is_err(), "refuses {cmd:?}");
        }
        for cmd in ["D", "R", "t", "u", "@", "AB", ":;", "V0099"] {
            assert!(check_cmd(cmd).is_ok(), "accepts {cmd:?}");
        }
        assert!(check_cmd("21").unwrap_err().contains("hard power-off"));
        assert!(check_cmd("V10").is_ok() && check_cmd("V1x").is_err());
        assert!(check_cmd(&"2".repeat(MAX_CMD + 1)).is_err());
        assert!(
            check_cmd("2\n3").is_err(),
            "no second line through the FIFO"
        );
    }

    #[test]
    fn fifo_send() {
        let d = tempfile::tempdir().unwrap();
        let fifo = d.path().join("cmd");
        // no FIFO: synohwd is not running
        assert!(matches!(
            send(&fifo, "2"),
            Err(ControlError::Unavailable(_))
        ));
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: plain mkfifo on a path we own
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // FIFO without a reader: ENXIO
        assert!(matches!(
            send(&fifo, "2"),
            Err(ControlError::Unavailable(_))
        ));
        // a reader like synohwd's (O_RDWR keeps it open)
        let mut r = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        send(&fifo, BEEP_LONG).unwrap();
        assert!(matches!(send(&fifo, "1"), Err(ControlError::Invalid(_))));
        let mut buf = [0u8; 16];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"3\n");
        // a regular file is refused
        let plain = d.path().join("plain");
        std::fs::write(&plain, "").unwrap();
        assert!(matches!(send(&plain, "2"), Err(ControlError::Failed(_))));
    }
}
