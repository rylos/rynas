//! A minimal client of NUT's upsd network protocol
//! (<https://networkupstools.org/docs/developer-guide.chunked/net-protocol.html>), shared by
//! rynas-web (status, no login) and rynasd (battery tests, the on-battery watcher).
//!
//! - Read side, no login: `LIST UPS`, `LIST VAR <ups>`, `LIST CMD <ups>` → [`read`] gives a
//!   [`UpsStatus`] of the first UPS, with the battery test commands it offers.
//! - Write side: `USERNAME`, `PASSWORD`, `INSTCMD <ups> <cmd>` ([`Conn::login`],
//!   [`Conn::instcmd`]); the user and its `instcmds` live in `upsd.users`.
//!
//! No `upsc`/`upscmd` child process; every conversation is bounded by [`TIMEOUT`] and answers
//! are capped at [`MAX_ANSWER`]. Talking to upsd never touches a disk.

pub mod setup;

use std::time::Duration;

use rynas_api_types::ups::{UpsLink, UpsStatus};
use tokio::net::TcpStream;

/// upsd's default address.
pub const UPSD: &str = "127.0.0.1:3493";
/// A whole status conversation (connect, three lists, logout).
pub const TIMEOUT: Duration = Duration::from_secs(2);
/// Largest answer read from upsd (a UPS has well under 100 variables).
pub const MAX_ANSWER: usize = 64 << 10;
/// Prefix of the instant commands kept in [`UpsStatus::commands`].
pub const TEST_PREFIX: &str = "test.battery.";

// ---------------------------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------------------------

/// One NUT answer line split into words; quoted words unescaped (`\"`, `\\`).
pub fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = line.chars().peekable();
    loop {
        while it.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            it.next();
        }
        let Some(&c) = it.peek() else { break };
        let mut w = String::new();
        if c == '"' {
            it.next();
            while let Some(c) = it.next() {
                match c {
                    '\\' => {
                        if let Some(n) = it.next() {
                            w.push(n);
                        }
                    }
                    '"' => break,
                    c => w.push(c),
                }
            }
        } else {
            while let Some(&c) = it.peek() {
                if c.is_ascii_whitespace() {
                    break;
                }
                w.push(c);
                it.next();
            }
        }
        out.push(w);
    }
    out
}

/// A UPS, user or command name that can be sent unquoted (no space, quote, backslash or
/// control character; not empty, at most 64 bytes).
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\' && b != b'#')
}

/// UPS names of a `LIST UPS` answer (`UPS <name> "<description>"` lines).
pub fn parse_list_ups(text: &str) -> Vec<String> {
    text.lines()
        .map(words)
        .filter(|w| w.len() >= 2 && w[0] == "UPS")
        .map(|mut w| w.swap_remove(1))
        .collect()
}

/// `(variable, value)` pairs of a `LIST VAR <ups>` answer (`VAR <ups> <var> "<value>"`).
pub fn parse_list_var(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(words)
        .filter(|w| w.len() >= 4 && w[0] == "VAR")
        .map(|mut w| {
            let v = w.swap_remove(3);
            (w.swap_remove(2), v)
        })
        .collect()
}

/// Command names of a `LIST CMD <ups>` answer (`CMD <ups> <cmd>`).
pub fn parse_list_cmd(text: &str) -> Vec<String> {
    text.lines()
        .map(words)
        .filter(|w| w.len() >= 3 && w[0] == "CMD")
        .map(|mut w| w.swap_remove(2))
        .collect()
}

/// The status of UPS `name` from its variables and commands.
pub fn status_of(
    name: &str,
    vars: &[(String, String)],
    commands: &[String],
    now_ms: u64,
) -> UpsStatus {
    let get = |k: &str| {
        vars.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let num = |k: &str| get(k).and_then(|v| v.parse::<f32>().ok());
    let mut commands: Vec<String> = commands
        .iter()
        .filter(|c| c.starts_with(TEST_PREFIX))
        .cloned()
        .collect();
    commands.sort();
    commands.dedup();
    UpsStatus {
        link: UpsLink::Connected,
        name: Some(name.to_string()),
        flags: get("ups.status")
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        battery_charge_pct: num("battery.charge").map(|v| v.clamp(0.0, 100.0)),
        battery_runtime_s: num("battery.runtime")
            .filter(|v| *v >= 0.0)
            .map(|v| v as u64),
        load_pct: num("ups.load").map(|v| v.max(0.0)),
        input_voltage_v: num("input.voltage"),
        output_voltage_v: num("output.voltage"),
        manufacturer: get("device.mfr").or_else(|| get("ups.mfr")),
        model: get("device.model").or_else(|| get("ups.model")),
        serial: get("device.serial").or_else(|| get("ups.serial")),
        beeper: get("ups.beeper.status"),
        battery_date: get("battery.date").or_else(|| get("battery.mfr.date")),
        test_result: get("ups.test.result"),
        driver: get("driver.name"),
        commands,
        read_at_ms: now_ms,
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// ---------------------------------------------------------------------------------------------
// the connection
// ---------------------------------------------------------------------------------------------

/// One connection to upsd. Callers bound the whole conversation with a timeout.
pub struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
}

/// An `ERR <code>` line of upsd (e.g. `ACCESS-DENIED`, `CMD-NOT-SUPPORTED`) as an error.
fn upsd_err(line: &str) -> std::io::Error {
    std::io::Error::other(line.trim_start_matches("ERR ").to_string())
}

impl Conn {
    pub async fn connect(addr: &str) -> std::io::Result<Self> {
        Ok(Self {
            s: TcpStream::connect(addr).await?,
            buf: Vec::new(),
        })
    }

    async fn send(&self, cmd: &str) -> std::io::Result<()> {
        let mut out = cmd.as_bytes();
        while !out.is_empty() {
            self.s.writable().await?;
            match self.s.try_write(out) {
                Ok(n) => out = &out[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn fill(&mut self) -> std::io::Result<()> {
        if self.buf.len() > MAX_ANSWER {
            return Err(std::io::Error::other("answer too long"));
        }
        let mut chunk = [0u8; 4096];
        loop {
            self.s.readable().await?;
            match self.s.try_read(&mut chunk) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// The next answer line (without `\n`).
    async fn line(&mut self) -> std::io::Result<String> {
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let l: Vec<u8> = self.buf.drain(..=i).collect();
                return Ok(String::from_utf8_lossy(&l[..i])
                    .trim_end_matches('\r')
                    .into());
            }
            self.fill().await?;
        }
    }

    /// Sends `cmd` and reads one line: `OK…` or an `ERR` error.
    pub async fn simple(&mut self, cmd: &str) -> std::io::Result<String> {
        self.send(cmd).await?;
        let l = self.line().await?;
        if l.starts_with("ERR") {
            return Err(upsd_err(&l));
        }
        Ok(l)
    }

    /// Sends a `LIST …` command and reads until `END LIST …` (or an `ERR` line).
    pub async fn list(&mut self, what: &str) -> std::io::Result<String> {
        self.send(&format!("LIST {what}\n")).await?;
        let end = format!("END LIST {what}");
        let mut out = String::new();
        loop {
            let l = self.line().await?;
            if l.starts_with("ERR") {
                return Err(upsd_err(&l));
            }
            if l == end {
                return Ok(out);
            }
            if out.len() > MAX_ANSWER {
                return Err(std::io::Error::other("answer too long"));
            }
            out.push_str(&l);
            out.push('\n');
        }
    }

    /// The first UPS, `None` when upsd has none.
    pub async fn first_ups(&mut self) -> std::io::Result<Option<String>> {
        let name = parse_list_ups(&self.list("UPS").await?).into_iter().next();
        match name {
            Some(n) if !valid_name(&n) => Err(std::io::Error::other("invalid UPS name")),
            n => Ok(n),
        }
    }

    pub async fn vars(&mut self, ups: &str) -> std::io::Result<Vec<(String, String)>> {
        Ok(parse_list_var(&self.list(&format!("VAR {ups}")).await?))
    }

    pub async fn commands(&mut self, ups: &str) -> std::io::Result<Vec<String>> {
        Ok(parse_list_cmd(&self.list(&format!("CMD {ups}")).await?))
    }

    /// `USERNAME` + `PASSWORD`. The password is never part of an error or a log line.
    pub async fn login(&mut self, user: &str, password: &str) -> std::io::Result<()> {
        if !valid_name(user) || !valid_name(password) {
            return Err(std::io::Error::other("invalid upsd user or password"));
        }
        self.simple(&format!("USERNAME {user}\n")).await?;
        self.simple(&format!("PASSWORD {password}\n"))
            .await
            .map_err(|e| std::io::Error::other(format!("upsd refused the password: {e}")))?;
        Ok(())
    }

    /// `INSTCMD <ups> <cmd>`.
    pub async fn instcmd(&mut self, ups: &str, cmd: &str) -> std::io::Result<()> {
        if !valid_name(ups) || !valid_name(cmd) {
            return Err(std::io::Error::other("invalid UPS or command name"));
        }
        self.simple(&format!("INSTCMD {ups} {cmd}\n"))
            .await
            .map(drop)
    }

    pub async fn logout(mut self) {
        let _ = self.simple("LOGOUT\n").await;
    }
}

async fn converse(addr: &str) -> std::io::Result<UpsStatus> {
    let mut c = Conn::connect(addr).await?;
    let Some(name) = c.first_ups().await? else {
        return Ok(UpsStatus {
            link: UpsLink::NotConfigured,
            read_at_ms: now_ms(),
            ..Default::default()
        });
    };
    let vars = c.vars(&name).await?;
    // an upsd that refuses LIST CMD (access rules) still gives a status
    let cmds = match c.commands(&name).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("upsd {addr}: LIST CMD {name}: {e}");
            Vec::new()
        }
    };
    let _ = c.send("LOGOUT\n").await;
    Ok(status_of(&name, &vars, &cmds, now_ms()))
}

/// One reading of upsd at `addr`; an unreachable or broken upsd is [`UpsLink::Unreachable`].
pub async fn read(addr: &str) -> UpsStatus {
    let down = || UpsStatus {
        read_at_ms: now_ms(),
        ..Default::default()
    };
    match tokio::time::timeout(TIMEOUT, converse(addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!("upsd {addr}: {e}");
            down()
        }
        Err(_) => {
            tracing::debug!("upsd {addr}: no answer in {TIMEOUT:?}");
            down()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const VARS: &str = "BEGIN LIST VAR ups\n\
        VAR ups battery.charge \"87\"\n\
        VAR ups battery.runtime \"2460\"\n\
        VAR ups battery.mfr.date \"2023/04/12\"\n\
        VAR ups device.mfr \"American Power Conversion\"\n\
        VAR ups device.model \"Back-UPS ES 700G\"\n\
        VAR ups device.serial \"5B1234X56789  \"\n\
        VAR ups driver.name \"usbhid-ups\"\n\
        VAR ups input.voltage \"229.0\"\n\
        VAR ups ups.beeper.status \"enabled\"\n\
        VAR ups ups.load \"18\"\n\
        VAR ups ups.status \"OB DISCHRG\"\n\
        VAR ups ups.test.result \"Done and \\\"passed\\\"\"\n\
        END LIST VAR ups\n";

    const CMDS: &str = "BEGIN LIST CMD ups\nCMD ups beeper.disable\nCMD ups test.battery.start.quick\n\
        CMD ups test.battery.stop\nEND LIST CMD ups\n";

    #[test]
    fn parses_lists() {
        assert_eq!(
            parse_list_ups("BEGIN LIST UPS\nUPS ups \"APC on USB\"\nEND LIST UPS\n"),
            ["ups"]
        );
        assert!(parse_list_ups("BEGIN LIST UPS\nEND LIST UPS\n").is_empty());
        let v = parse_list_var(VARS);
        assert_eq!(v.len(), 12);
        let c = parse_list_cmd(CMDS);
        assert_eq!(c.len(), 3);
        let s = status_of("ups", &v, &c, 5);
        assert_eq!(s.link, UpsLink::Connected);
        assert_eq!(s.flags, ["OB", "DISCHRG"]);
        assert!(s.on_battery() && !s.low_battery());
        assert_eq!(s.battery_charge_pct, Some(87.0));
        assert_eq!(s.battery_runtime_s, Some(2460));
        assert_eq!(s.load_pct, Some(18.0));
        assert_eq!(s.input_voltage_v, Some(229.0));
        assert_eq!(s.output_voltage_v, None);
        assert_eq!(s.model.as_deref(), Some("Back-UPS ES 700G"));
        assert_eq!(s.serial.as_deref(), Some("5B1234X56789"));
        assert_eq!(s.battery_date.as_deref(), Some("2023/04/12"));
        assert_eq!(s.beeper.as_deref(), Some("enabled"));
        assert_eq!(s.test_result.as_deref(), Some("Done and \"passed\""));
        // only the battery tests, sorted
        assert_eq!(
            s.commands,
            ["test.battery.start.quick", "test.battery.stop"]
        );
        assert_eq!(s.read_at_ms, 5);
    }

    #[test]
    fn names() {
        assert!(valid_name("ups") && valid_name("test.battery.start.quick"));
        assert!(!valid_name("") && !valid_name("a b") && !valid_name("a\"") && !valid_name("x\n"));
    }

    /// A scripted upsd: answers each request line with `reply(line)`; records the lines.
    pub async fn fake_upsd(
        reply: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let reply = std::sync::Arc::new(reply);
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                let (reply, seen) = (reply.clone(), seen2.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 256];
                    loop {
                        if s.readable().await.is_err() {
                            return;
                        }
                        match s.try_read(&mut chunk) {
                            Ok(0) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(_) => continue,
                        }
                        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=i).collect();
                            let line = String::from_utf8_lossy(&line[..i]).to_string();
                            seen.lock().unwrap().push(line.clone());
                            let r = reply(&line);
                            let _ = s.writable().await;
                            let _ = s.try_write(r.as_bytes());
                        }
                    }
                });
            }
        });
        (addr, seen)
    }

    fn upsd(with_ups: bool) -> impl Fn(&str) -> String + Send + Sync + 'static {
        move |line: &str| match line {
            "LIST UPS" if with_ups => "BEGIN LIST UPS\nUPS ups \"x\"\nEND LIST UPS\n".into(),
            "LIST UPS" => "BEGIN LIST UPS\nEND LIST UPS\n".into(),
            "LIST VAR ups" => VARS.into(),
            "LIST CMD ups" => CMDS.into(),
            "USERNAME rynas" => "OK\n".into(),
            "PASSWORD good" => "OK\n".into(),
            l if l.starts_with("PASSWORD") => "ERR INVALID-PASSWORD\n".into(),
            "INSTCMD ups test.battery.start.quick" => "OK\n".into(),
            l if l.starts_with("INSTCMD") => "ERR CMD-NOT-SUPPORTED\n".into(),
            "LOGOUT" => "OK Goodbye\n".into(),
            _ => "ERR UNKNOWN-COMMAND\n".into(),
        }
    }

    #[tokio::test]
    async fn reads_a_fake_upsd() {
        let (addr, _) = fake_upsd(upsd(true)).await;
        let s = read(&addr).await;
        assert_eq!(s.link, UpsLink::Connected);
        assert_eq!(s.battery_charge_pct, Some(87.0));
        assert_eq!(s.commands.len(), 2);
        let (addr, _) = fake_upsd(upsd(false)).await;
        assert_eq!(read(&addr).await.link, UpsLink::NotConfigured);
        // LIST CMD refused: a status without commands
        let (addr, _) = fake_upsd(|l: &str| {
            if l == "LIST CMD ups" {
                "ERR ACCESS-DENIED\n".into()
            } else {
                upsd(true)(l)
            }
        })
        .await;
        let s = read(&addr).await;
        assert_eq!(s.link, UpsLink::Connected);
        assert!(s.commands.is_empty());
        // nothing listens there
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = l.local_addr().unwrap().to_string();
        drop(l);
        assert_eq!(read(&dead).await.link, UpsLink::Unreachable);
    }

    #[tokio::test]
    async fn instant_commands() {
        let (addr, seen) = fake_upsd(upsd(true)).await;
        let mut c = Conn::connect(&addr).await.unwrap();
        assert_eq!(c.first_ups().await.unwrap().as_deref(), Some("ups"));
        let e = c.login("rynas", "bad").await.unwrap_err().to_string();
        assert!(e.contains("INVALID-PASSWORD") && !e.contains("bad"), "{e}");
        let mut c = Conn::connect(&addr).await.unwrap();
        c.login("rynas", "good").await.unwrap();
        c.instcmd("ups", "test.battery.start.quick").await.unwrap();
        let e = c
            .instcmd("ups", "test.battery.start.deep")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("CMD-NOT-SUPPORTED"));
        assert!(c.instcmd("ups", "x y").await.is_err());
        c.logout().await;
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"INSTCMD ups test.battery.start.quick".to_string()));
        assert!(!seen.iter().any(|l| l.contains("x y")));
    }
}
