//! The restore TUI (ratatui): NAS → certificate → method → restore code, or administrator +
//! TOTP and the PC from a list → version →
//! target disk → passphrase (LUKS) → plan → the disk name typed → restore (plain log in
//! the terminal) → reboot. Esc goes back one step, Ctrl-C leaves.

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::source::RestoreSource;
use super::{exec, plan, restic};
use crate::http::{self, Client, Endpoint};

enum Answer<T> {
    Ok(T),
    Back,
    Quit,
}

fn frame_title(t: &str) -> Block<'_> {
    Block::default().borders(Borders::ALL).title(format!(" rynas-agent restore · {t} "))
}

fn key() -> Option<(KeyCode, KeyModifiers)> {
    if event::poll(Duration::from_millis(250)).ok()?
        && let Ok(Event::Key(k)) = event::read()
        && k.kind == KeyEventKind::Press
    {
        return Some((k.code, k.modifiers));
    }
    None
}

fn input(term: &mut DefaultTerminal, title: &str, help: &[String], initial: &str, masked: bool) -> Answer<String> {
    let mut s = initial.to_string();
    loop {
        let _ = term.draw(|f| {
            let [top, field] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(f.area());
            let lines: Vec<Line> = help.iter().map(|l| Line::from(l.as_str())).collect();
            f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(frame_title(title)), top);
            let shown = if masked { "•".repeat(s.chars().count()) } else { s.clone() };
            f.render_widget(Paragraph::new(format!("{shown}▏")).block(Block::default().borders(Borders::ALL).title(" Enter: next · Esc: back ")), field);
        });
        match key() {
            Some((KeyCode::Char('c'), m)) if m.contains(KeyModifiers::CONTROL) => return Answer::Quit,
            Some((KeyCode::Enter, _)) => return Answer::Ok(s.trim().to_string()),
            Some((KeyCode::Esc, _)) => return Answer::Back,
            Some((KeyCode::Backspace, _)) => {
                s.pop();
            }
            Some((KeyCode::Char(c), _)) => s.push(c),
            _ => {}
        }
    }
}

/// A list; `None` entries in `enabled` cannot be picked (shown dim with the reason).
fn choose(term: &mut DefaultTerminal, title: &str, intro: &[String], items: &[(String, Option<String>)]) -> Answer<usize> {
    let mut st = ListState::default();
    st.select(items.iter().position(|(_, why)| why.is_none()));
    loop {
        let _ = term.draw(|f| {
            let h = (intro.len() as u16 + 2).min(f.area().height / 2);
            let [top, list] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(f.area());
            let lines: Vec<Line> = intro.iter().map(|l| Line::from(l.as_str())).collect();
            f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(frame_title(title)), top);
            let li: Vec<ListItem> = items
                .iter()
                .map(|(t, why)| match why {
                    None => ListItem::new(t.as_str()),
                    Some(w) => ListItem::new(format!("{t}  ({w})")).style(Style::default().add_modifier(Modifier::DIM)),
                })
                .collect();
            let l = List::new(li)
                .block(Block::default().borders(Borders::ALL).title(" ↑↓ choose · Enter: next · Esc: back "))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            f.render_stateful_widget(l, list, &mut st);
        });
        match key() {
            Some((KeyCode::Char('c'), m)) if m.contains(KeyModifiers::CONTROL) => return Answer::Quit,
            Some((KeyCode::Esc, _)) => return Answer::Back,
            Some((KeyCode::Up, _)) => {
                let i = st.selected().unwrap_or(0);
                st.select(Some(i.saturating_sub(1)));
            }
            Some((KeyCode::Down, _)) => {
                let i = st.selected().unwrap_or(0);
                st.select(Some((i + 1).min(items.len().saturating_sub(1))));
            }
            Some((KeyCode::Enter, _)) => {
                if let Some(i) = st.selected()
                    && items.get(i).is_some_and(|(_, why)| why.is_none())
                {
                    return Answer::Ok(i);
                }
            }
            _ => {}
        }
    }
}

fn message(term: &mut DefaultTerminal, title: &str, lines: &[String]) -> Answer<()> {
    loop {
        let _ = term.draw(|f| {
            let mut l: Vec<Line> = lines.iter().map(|x| Line::from(x.as_str())).collect();
            l.push(Line::from(""));
            l.push(Line::from("Enter: continue · Esc: back").italic());
            f.render_widget(Paragraph::new(Text::from(l)).wrap(Wrap { trim: false }).block(frame_title(title)), f.area());
        });
        match key() {
            Some((KeyCode::Char('c'), m)) if m.contains(KeyModifiers::CONTROL) => return Answer::Quit,
            Some((KeyCode::Enter, _)) => return Answer::Ok(()),
            Some((KeyCode::Esc, _)) => return Answer::Back,
            _ => {}
        }
    }
}

fn date(ms: u64) -> String {
    // UTC, without a time zone database on a live system
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, (rem % 3600) / 60)
}

fn gib(b: u64) -> String {
    format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
}

macro_rules! step {
    ($e:expr $(, $back:expr)?) => {
        match $e {
            Answer::Ok(v) => v,
            #[allow(unreachable_code)]
            Answer::Back => {
                $($back;)?
                continue;
            }
            Answer::Quit => return Err("stopped".into()),
        }
    };
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut term = ratatui::init();
    let r = flow(&mut term, args);
    ratatui::restore();
    let job = r?;
    let Some((mut src, v, m, p, pass)) = job else {
        return Ok(());
    };
    // the work itself, with a plain log
    println!("Restoring {} version {} …", src.device(), &v.id[..8]);
    let mut log = exec::Log {
        lines: Vec::new(),
        echo: Box::new(|s| println!("{s}")),
    };
    let r = exec::execute(&mut src, &v, &m, &p, &exec::Options { passphrase: pass.as_deref() }, &mut log);
    let text = log.text();
    let cut = text.len().saturating_sub(60 << 10);
    let cut = (cut..text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
    match &r {
        Ok(()) => src.report(&v, true, "restored", &text[cut..]),
        Err(e) => src.report(&v, false, e, &text[cut..]),
    }
    r?;
    println!("\nThe PC is restored. Remove the live medium and reboot.");
    eprint!("Reboot now? [y/N] ");
    let mut a = String::new();
    let _ = std::io::stdin().read_line(&mut a);
    if a.trim().eq_ignore_ascii_case("y") {
        let _ = std::process::Command::new("systemctl").arg("reboot").status().or_else(|_| std::process::Command::new("reboot").status());
    }
    Ok(())
}

type Job = (restic::ResticSource, super::source::Version, rynas_api_types::pcagent::Manifest, plan::Plan, Option<String>);

fn flow(term: &mut DefaultTerminal, args: &[String]) -> Result<Option<Job>, String> {
    let mut stage = 0;
    let mut url = super::opt(args, "--nas").unwrap_or_default();
    let mut ep: Option<Endpoint> = None;
    let mut fp = String::new();
    let mut src: Option<restic::ResticSource> = None;
    let mut admin_user = String::new();
    let mut choice: Vec<rynas_api_types::pcagent::RestoreChoiceDevice> = Vec::new();
    let mut versions = Vec::new();
    let mut version: Option<super::source::Version> = None;
    let mut manifest: Option<rynas_api_types::pcagent::Manifest> = None;
    let mut target: Option<plan::TargetDisk> = None;
    let mut pass: Option<String> = None;
    let mut the_plan: Option<plan::Plan> = None;
    let mut error: Option<String> = None;
    loop {
        let err_lines = |e: &Option<String>| -> Vec<String> { e.iter().map(|x| format!("! {x}")).collect() };
        match stage {
            0 => {
                let mut help = vec![
                    "Bare-metal restore of a PC from its backup on the NAS.".to_string(),
                    "The target disk is erased; the PC comes back as it was in the chosen version.".into(),
                    String::new(),
                    "NAS address, as on the PC page (https://nas.local:8443):".into(),
                ];
                help.extend(err_lines(&error));
                url = step!(input(term, "NAS", &help, &url, false), return Ok(None));
                match Endpoint::parse(&url) {
                    Ok(e) if !e.tls && !super::flag(args, "--allow-http") => error = Some("use https (or start with --allow-http on a trusted link)".into()),
                    Ok(e) => {
                        ep = Some(e);
                        error = None;
                        stage = 1;
                    }
                    Err(e) => error = Some(e),
                }
            }
            1 => {
                let e = ep.clone().ok_or("no NAS")?;
                if !e.tls {
                    fp.clear();
                    stage = 2;
                    continue;
                }
                let probe = Client::new(e.clone(), "");
                let _ = probe.request("GET", "/api/v1/auth/session", &[], &[]);
                let Some(seen) = probe.seen_fingerprint() else {
                    error = Some(format!("{}: no answer", e.url()));
                    stage = 0;
                    continue;
                };
                if let Some(given) = super::opt(args, "--fingerprint")
                    && http::normalize_fp(&given) == seen
                {
                    fp = seen;
                    stage = 2;
                    continue;
                }
                let lines = vec![
                    "The NAS answers with this certificate (SHA-256):".to_string(),
                    String::new(),
                    seen.clone(),
                    String::new(),
                    "Compare it with the fingerprint on the PC page of the NAS. Same? Enter.".into(),
                ];
                step!(message(term, "Certificate", &lines), stage = 0);
                fp = seen;
                stage = 2;
            }
            2 => {
                let items = vec![
                    ("File backup: the PC's repository on the NAS".to_string(), None),
                    ("btrfs snapshots received by the NAS".to_string(), Some("not in this build of the agent".to_string())),
                ];
                let intro = vec!["Which backup of the PC?".to_string()];
                step!(choose(term, "Method", &intro, &items), stage = 1);
                stage = 3;
            }
            3 => {
                let items = vec![
                    ("Restore code, made on the PC page of the NAS".to_string(), None),
                    ("Administrator of the NAS with TOTP".to_string(), None),
                ];
                let intro = vec!["How do you log in to the NAS?".to_string()];
                let i = step!(choose(term, "Access", &intro, &items), stage = 2);
                error = None;
                stage = if i == 0 { 30 } else { 31 };
            }
            31 => {
                let mut help = vec![
                    "An administrator account of the NAS with TOTP turned on (without TOTP it is refused).".to_string(),
                    "It can restore any PC; the codes it gets last 5 minutes.".into(),
                    String::new(),
                    "User name:".into(),
                ];
                help.extend(err_lines(&error));
                admin_user = step!(input(term, "Administrator", &help, &admin_user, false), stage = 3);
                let password = step!(input(term, "Password", &[format!("Password of {admin_user}:")], "", true), stage = 3);
                let totp = step!(input(term, "TOTP", &["The 6-digit code of the authenticator app, or a recovery code:".to_string()], "", false), stage = 3);
                match restic::ResticSource::admin_choice(&Client::new(ep.clone().ok_or("no NAS")?, &fp), &admin_user, &password, &totp) {
                    Ok(c) if !c.devices.is_empty() => {
                        choice = c.devices;
                        error = None;
                        stage = 32;
                    }
                    Ok(_) => error = Some("the NAS has no PC with backups".into()),
                    Err(e) => error = Some(e),
                }
            }
            32 => {
                let items: Vec<(String, Option<String>)> = choice
                    .iter()
                    .map(|d| {
                        let mut s = format!("{}  {}", d.device, d.hostname);
                        if !d.description.is_empty() {
                            s.push_str(&format!("  {}", d.description));
                        }
                        (s, None)
                    })
                    .collect();
                let mut intro = vec!["Which PC?".to_string()];
                intro.extend(err_lines(&error));
                let i = step!(choose(term, "PC", &intro, &items), stage = 31);
                let d = choice[i].clone();
                match restic::ResticSource::login(Client::new(ep.clone().ok_or("no NAS")?, &fp), &d.code, Some(&d.device)) {
                    Ok(mut s) => match s.versions() {
                        Ok(v) if !v.is_empty() => {
                            versions = v;
                            src = Some(s);
                            error = None;
                            stage = 4;
                        }
                        // the batch is used up: log in again for another PC
                        Ok(_) => {
                            error = Some(format!("{} has no version yet; log in again", d.device));
                            stage = 31;
                        }
                        Err(e) => {
                            error = Some(e);
                            stage = 31;
                        }
                    },
                    Err(e) => {
                        error = Some(format!("{e} (the codes last 5 minutes: log in again)"));
                        stage = 31;
                    }
                }
            }
            30 => {
                let mut help = vec![
                    "Restore code: made on the PC page of the NAS (Restore → Restore code).".to_string(),
                    "It works once, for one hour, and only reads.".into(),
                ];
                help.extend(err_lines(&error));
                let code = step!(input(term, "Restore code", &help, "", false), stage = 3);
                match restic::ResticSource::login(Client::new(ep.clone().ok_or("no NAS")?, &fp), &code, None) {
                    Ok(mut s) => match s.versions() {
                        Ok(v) if !v.is_empty() => {
                            versions = v;
                            src = Some(s);
                            error = None;
                            stage = 4;
                        }
                        Ok(_) => error = Some("the PC has no version yet".into()),
                        Err(e) => error = Some(e),
                    },
                    Err(e) => error = Some(e),
                }
            }
            4 => {
                let s = src.as_ref().ok_or("no source")?;
                let items: Vec<(String, Option<String>)> = versions
                    .iter()
                    .map(|v| (format!("{}  {}  {} files  {}", date(v.time_ms), &v.id[..8], v.files, gib(v.bytes)), None))
                    .collect();
                let intro = vec![format!("PC {} ({}): choose the version", s.device(), s.dev.hostname)];
                let i = step!(choose(term, "Version", &intro, &items), stage = 3);
                let v = versions[i].clone();
                match src.as_mut().ok_or("no source")?.manifest(&v) {
                    Ok(m) => {
                        manifest = Some(m);
                        version = Some(v);
                        error = None;
                        stage = 5;
                    }
                    Err(e) => {
                        step!(message(term, "Version", &[e]));
                    }
                }
            }
            5 => {
                let m = manifest.as_ref().ok_or("no manifest")?;
                let src_index = super::source_disk(m).unwrap_or(0);
                let src_disk = m.disks.get(src_index);
                let mut intro = vec![match src_disk {
                    Some(d) => format!("The PC's disk: {} {} {} ({}). Choose the target disk: it will be ERASED.", d.name, d.model, d.serial, gib(d.size_bytes)),
                    None => "Choose the target disk: it will be ERASED.".into(),
                }];
                intro.extend(err_lines(&error));
                let disks = super::target_disks();
                let items: Vec<(String, Option<String>)> = disks
                    .iter()
                    .map(|d| {
                        let same = src_disk.is_some_and(|s| !s.serial.is_empty() && s.serial == d.serial);
                        (
                            format!("/dev/{}  {}  {}  {}{}", d.name, d.model, d.serial, gib(d.size_bytes), if same { "  (the PC's own disk)" } else { "" }),
                            exec::disk_in_use(&d.name),
                        )
                    })
                    .collect();
                if items.is_empty() {
                    return Err("no disk found".into());
                }
                let i = step!(choose(term, "Target disk", &intro, &items), stage = 4);
                let t = disks[i].clone();
                let names = vec![t.name.clone()];
                match plan::build(m, &[(src_index, t.clone())], &super::in_use_except(&names)) {
                    Ok(p) => {
                        let problems = super::preflight(m, &p);
                        if !problems.is_empty() {
                            step!(message(term, "Before restoring", &problems));
                            continue;
                        }
                        target = Some(t);
                        the_plan = Some(p);
                        error = None;
                        stage = 6;
                    }
                    Err(e) => error = Some(e),
                }
            }
            6 => {
                let p = the_plan.as_ref().ok_or("no plan")?;
                if p.luks.is_empty() {
                    stage = 7;
                    continue;
                }
                let mut help = vec!["The PC used LUKS encryption: the container is made again with its UUID.".to_string(), "New passphrase:".into()];
                help.extend(err_lines(&error));
                let a = step!(input(term, "LUKS passphrase", &help, "", true), stage = 5);
                let b = step!(input(term, "LUKS passphrase", &["The same passphrase again:".to_string()], "", true));
                if a.is_empty() || a != b {
                    error = Some("the two passphrases differ (or are empty)".into());
                    continue;
                }
                pass = Some(a);
                error = None;
                stage = 7;
            }
            7 => {
                let (m, p, t) = (manifest.as_ref().ok_or("no manifest")?, the_plan.as_ref().ok_or("no plan")?, target.as_ref().ok_or("no target")?);
                let mut lines = plan::describe(m, p);
                let reasons = m.ready_reasons();
                if !reasons.is_empty() {
                    lines.push(format!("readiness notes: {reasons:?}"));
                }
                lines.push(String::new());
                lines.push(format!("/dev/{} ({} {}) WILL BE ERASED. Type its name ({}) to start:", t.name, t.model, t.serial, t.name));
                lines.extend(err_lines(&error));
                let typed = step!(input(term, "Plan", &lines, "", false), stage = if p.luks.is_empty() { 5 } else { 6 });
                if typed.trim_start_matches("/dev/") != t.name {
                    error = Some("that is not the disk's name".into());
                    continue;
                }
                let s = src.take().ok_or("no source")?;
                return Ok(Some((s, version.take().ok_or("no version")?, manifest.take().ok_or("no manifest")?, the_plan.take().ok_or("no plan")?, pass.take())));
            }
            _ => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::date(1_709_210_096_000), "2024-02-29 12:34 UTC");
    }
}
