//! The restore TUI (ratatui): NAS → certificate → restore code, or administrator + TOTP
//! and the PC from a list → method (when the PC has both) → version (file method) or run of
//! btrfs snapshots → mode (whole / system / custom / add) and subvolumes with their
//! snapshots → target disk (or, adding, the PC's btrfs filesystem) → passphrase (LUKS) →
//! plan → the disk name typed → restore (plain log in the terminal) → reboot, offered only
//! when the checks after the restore passed. `--dry-run` shows the plan and the text fixes
//! on copies, and writes nothing. Esc goes back one step, Ctrl-C leaves.

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use std::sync::Arc;

use rynas_api_types::pcagent::{Manifest, RestoreDevice};

use super::source::{RestoreSource, Version};
use super::{btrfs, exec, plan, restic};
use crate::http::{self, Client, Endpoint};

/// A step found a state the earlier steps always set (a bug): the TUI ends with this.
const INTERNAL: &str = "internal error of the restore TUI: start it again";

/// Where the NAS' web interface makes a restore code.
const CODE_WHERE: &str = "Backup → PCs → the PC → Restore code";

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

/// A readiness reason of the manifest as a sentence.
fn ready_text(r: rynas_api_types::pcagent::ReadyReason) -> &'static str {
    use rynas_api_types::pcagent::ReadyReason::*;
    match r {
        NoVersion => "the PC has no backup version yet",
        NoManifest => "the version has no layout manifest",
        ManifestStale => "the layout manifest is older than the backup: the disk layout may have changed since",
        VolumeSkipped => "a mounted filesystem of the PC is not in the backup: it comes back empty",
        LoaderUnknown => "the boot loader is unknown: the boot step may need a hand",
        NoPartitionTable => "no partition table could be read",
        NoRecoveryKit => "the backup group has no recovery kit yet",
        ManualLayout => "LVM or md: recreate them by hand, this restore does not",
    }
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
    let Some(job) = r? else {
        return Ok(());
    };
    // the work itself, with a plain log
    let mut log = exec::Log::new(Box::new(|s| println!("{s}")));
    let format = matches!(job, Job::Format { .. });
    match job {
        Job::Format { mut src, v, m, p, pass } => {
            println!("Restoring {} ({}) version {} …", src.device(), src.kind(), &v.id[..8.min(v.id.len())]);
            let r = exec::execute(src.as_mut(), &v, &m, &p, &exec::Options { passphrase: pass.as_deref(), no_nvram: args.iter().any(|a| a == "--no-nvram") }, &mut log);
            super::finish(src.as_mut(), &v, r, &log)?;
        }
        Job::Add { mut src, v, m, device, restored } => {
            println!("Adding {} to {device} …", restored.join(", "));
            let r = exec::execute_add(src.as_mut(), &v, &m, &device, &restored, false, &mut log);
            super::finish(src.as_mut(), &v, r, &log)?;
            println!("\nThe subvolumes were added.");
            return Ok(());
        }
    }
    if format && !log.problems.is_empty() {
        println!("\nThe data is restored, but do not reboot yet: check what is listed above.");
        return Ok(());
    }
    println!("\nThe PC is restored. Remove the live medium and reboot.");
    eprint!("Reboot now? [y/N] ");
    let mut a = String::new();
    let _ = std::io::stdin().read_line(&mut a);
    if a.trim().eq_ignore_ascii_case("y") {
        let _ = std::process::Command::new("systemctl").arg("reboot").status().or_else(|_| std::process::Command::new("reboot").status());
    }
    Ok(())
}

/// A checklist: Space ticks, `o` asks for another snapshot of the highlighted line
/// (`Ok(Some(i))`), Enter goes on (`Ok(None)`).
fn checklist(term: &mut DefaultTerminal, title: &str, intro: &[String], items: &[String], checked: &mut [bool]) -> Answer<Option<usize>> {
    let mut st = ListState::default();
    st.select(Some(0));
    loop {
        let _ = term.draw(|f| {
            let h = (intro.len() as u16 + 2).min(f.area().height / 2);
            let [top, list] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(f.area());
            let lines: Vec<Line> = intro.iter().map(|l| Line::from(l.as_str())).collect();
            f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(frame_title(title)), top);
            let li: Vec<ListItem> = items.iter().zip(checked.iter()).map(|(t, c)| ListItem::new(format!("[{}] {t}", if *c { "x" } else { " " }))).collect();
            let l = List::new(li)
                .block(Block::default().borders(Borders::ALL).title(" ↑↓ · Space: tick · o: another snapshot · Enter: next · Esc: back "))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            f.render_stateful_widget(l, list, &mut st);
        });
        match key() {
            Some((KeyCode::Char('c'), m)) if m.contains(KeyModifiers::CONTROL) => return Answer::Quit,
            Some((KeyCode::Esc, _)) => return Answer::Back,
            Some((KeyCode::Up, _)) => st.select(Some(st.selected().unwrap_or(0).saturating_sub(1))),
            Some((KeyCode::Down, _)) => st.select(Some((st.selected().unwrap_or(0) + 1).min(items.len().saturating_sub(1)))),
            Some((KeyCode::Char(' '), _)) => {
                if let Some(c) = st.selected().and_then(|i| checked.get_mut(i)) {
                    *c = !*c;
                }
            }
            Some((KeyCode::Char('o'), _)) => {
                if let Some(i) = st.selected() {
                    return Answer::Ok(Some(i));
                }
            }
            Some((KeyCode::Enter, _)) => return Answer::Ok(None),
            _ => {}
        }
    }
}

/// btrfs filesystems the live system sees (for "add subvolumes").
fn btrfs_devices() -> Vec<String> {
    std::process::Command::new("blkid")
        .args(["-t", "TYPE=btrfs", "-o", "device"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

enum Job {
    /// Format the target disk and restore.
    Format { src: Box<dyn RestoreSource>, v: Version, m: Manifest, p: plan::Plan, pass: Option<String> },
    /// btrfs: add subvolumes into the PC's existing filesystem, nothing formatted.
    Add { src: Box<btrfs::BtrfsSource>, v: Version, m: Manifest, device: String, restored: Vec<String> },
}

fn flow(term: &mut DefaultTerminal, args: &[String]) -> Result<Option<Job>, String> {
    let mut stage = 0;
    let mut url = super::opt(args, "--nas").unwrap_or_default();
    let mut ep: Option<Endpoint> = None;
    let mut fp = String::new();
    let mut session: Option<(Arc<Client>, RestoreDevice)> = None;
    let mut restic_src: Option<restic::ResticSource> = None;
    let mut bsrc: Option<btrfs::BtrfsSource> = None;
    let mut use_btrfs = false;
    let mut admin_user = String::new();
    let mut choice: Vec<rynas_api_types::pcagent::RestoreChoiceDevice> = Vec::new();
    let mut versions: Vec<Version> = Vec::new();
    let mut version: Option<Version> = None;
    let mut manifest: Option<Manifest> = None;
    // the manifest the plan is built from (btrfs: sizes of the chosen snapshots, guided layout)
    let mut planned: Option<Manifest> = None;
    let mut mode = btrfs::Mode::Whole;
    let mut checked: Vec<bool> = Vec::new();
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut override_for: Option<usize> = None;
    let mut target: Option<plan::TargetDisk> = None;
    let mut pass: Option<String> = None;
    let mut the_plan: Option<plan::Plan> = None;
    let mut error: Option<String> = None;
    let dry = super::flag(args, "--dry-run");
    loop {
        let err_lines = |e: &Option<String>| -> Vec<String> { e.iter().map(|x| format!("! {x}")).collect() };
        let layout = |m: &Option<Manifest>| -> Vec<rynas_api_types::pcagent::BtrbkSubvolume> { m.as_ref().and_then(|m| m.btrbk.as_ref()).map(|b| b.subvolumes.clone()).unwrap_or_default() };
        match stage {
            0 => {
                let mut help = vec![
                    "Bare-metal restore of a PC from its backup on the NAS.".to_string(),
                    "The target disk is erased (except when adding subvolumes); the PC comes back as it was in the chosen version.".into(),
                    String::new(),
                    "NAS address, as in the restore command of the NAS (https://nas.local:8443):".into(),
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
                let e = ep.clone().ok_or(INTERNAL)?;
                if !e.tls {
                    fp.clear();
                    stage = 3;
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
                    stage = 3;
                    continue;
                }
                let lines = vec![
                    "The NAS answers with this certificate (SHA-256):".to_string(),
                    String::new(),
                    seen.clone(),
                    String::new(),
                    format!("Compare it with the fingerprint the NAS shows ({CODE_WHERE}, or the pairing of the PC). Same? Enter."),
                ];
                step!(message(term, "Certificate", &lines), stage = 0);
                fp = seen;
                stage = 3;
            }
            3 => {
                let items = vec![
                    (format!("Restore code, made on the NAS ({CODE_WHERE})"), None),
                    ("Administrator of the NAS with TOTP".to_string(), None),
                ];
                let intro = vec!["How do you log in to the NAS?".to_string()];
                let i = step!(choose(term, "Access", &intro, &items), stage = 1);
                error = None;
                stage = if i == 0 { 30 } else { 31 };
            }
            31 => {
                let mut help = vec![
                    "An administrator account of the NAS with TOTP turned on (without TOTP it is refused).".to_string(),
                    "It can restore any PC; after the login, choose the PC within 5 minutes.".into(),
                    String::new(),
                    "User name:".into(),
                ];
                help.extend(err_lines(&error));
                admin_user = step!(input(term, "Administrator", &help, &admin_user, false), stage = 3);
                let password = step!(input(term, "Password", &[format!("Password of {admin_user}:")], "", true), stage = 3);
                let totp = step!(input(term, "TOTP", &["The 6-digit code of the authenticator app, or a recovery code:".to_string()], "", false), stage = 3);
                match restic::ResticSource::admin_choice(&Client::new(ep.clone().ok_or(INTERNAL)?, &fp), &admin_user, &password, &totp) {
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
                match super::login(Client::new(ep.clone().ok_or(INTERNAL)?, &fp), &d.code, Some(&d.device)) {
                    Ok(s) => {
                        session = Some(s);
                        error = None;
                        stage = 2;
                    }
                    Err(e) => {
                        error = Some(format!("{e} (the codes last 5 minutes: log in again)"));
                        stage = 31;
                    }
                }
            }
            30 => {
                let mut help = vec![
                    format!("Restore code: made on the NAS ({CODE_WHERE})."),
                    "Single use, valid for one hour, read-only.".into(),
                ];
                help.extend(err_lines(&error));
                let code = step!(input(term, "Restore code", &help, "", false), stage = 3);
                match super::login(Client::new(ep.clone().ok_or(INTERNAL)?, &fp), &code, None) {
                    Ok(s) => {
                        session = Some(s);
                        error = None;
                        stage = 2;
                    }
                    Err(e) => error = Some(e),
                }
            }
            2 => {
                let (client, dev) = session.clone().ok_or(INTERNAL)?;
                use_btrfs = if dev.has_files() && dev.btrfs {
                    let items = vec![
                        ("File backup: the PC's repository on the NAS".to_string(), None),
                        ("btrfs snapshots received by the NAS".to_string(), None),
                    ];
                    let mut intro = vec![format!("{} has both kinds of backup. Which one?", dev.device)];
                    intro.extend(err_lines(&error));
                    // the session is open: going back means a new code
                    step!(choose(term, "Method", &intro, &items), stage = 3) == 1
                } else {
                    dev.btrfs
                };
                let loaded = if use_btrfs {
                    btrfs::BtrfsSource::new(client, dev).and_then(|mut s| {
                        let v = s.versions()?;
                        bsrc = Some(s);
                        Ok(v)
                    })
                } else {
                    let mut s = restic::ResticSource::from_session(client, dev);
                    let v = s.versions();
                    restic_src = Some(s);
                    v
                };
                match loaded {
                    Ok(v) if !v.is_empty() => {
                        versions = v;
                        error = None;
                        stage = if use_btrfs { 40 } else { 4 };
                    }
                    Ok(_) => {
                        error = Some(if use_btrfs { "the NAS has no run with a complete snapshot of this PC yet" } else { "the PC has no file backup yet" }.into());
                        stage = 3;
                    }
                    Err(e) => {
                        error = Some(e);
                        stage = 3;
                    }
                }
            }
            4 => {
                let s = restic_src.as_ref().ok_or(INTERNAL)?;
                let items: Vec<(String, Option<String>)> = versions
                    .iter()
                    .map(|v| (format!("{}  {}  {} files  {}", date(v.time_ms), &v.id[..8.min(v.id.len())], v.files, gib(v.bytes)), None))
                    .collect();
                let intro = vec![format!("PC {} ({}): choose the version", s.device(), s.dev.hostname)];
                let i = step!(choose(term, "Version", &intro, &items), stage = 3);
                let v = versions[i].clone();
                match restic_src.as_mut().ok_or(INTERNAL)?.manifest(&v) {
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
            40 => {
                let s = bsrc.as_ref().ok_or(INTERNAL)?;
                let items: Vec<(String, Option<String>)> = versions
                    .iter()
                    .map(|v| {
                        let run = s.run(&v.id);
                        let size = if v.bytes > 0 { gib(v.bytes) } else { "size not measured".into() };
                        let state = match run {
                            Some(r) if r.incomplete => "incomplete (a receive was interrupted)".to_string(),
                            Some(r) if !r.lacks.is_empty() => format!("lacks {}", r.lacks.join(", ")),
                            _ => "complete".into(),
                        };
                        (format!("{}  {}  {}  {}", date(v.time_ms), v.id, size, state), None)
                    })
                    .collect();
                let mut intro = vec![format!("PC {}: choose the run of btrfs snapshots", s.device())];
                if s.listing.manifest_snapshot.is_none() {
                    intro.push("no layout manifest on the NAS: an older snapshot's own files are read (legacy)".into());
                }
                intro.extend(err_lines(&error));
                let i = step!(choose(term, "Version", &intro, &items), stage = 3);
                let v = versions[i].clone();
                match bsrc.as_mut().ok_or(INTERNAL)?.manifest(&v) {
                    Ok(m) => {
                        checked = vec![true; layout(&Some(m.clone())).len()];
                        overrides.clear();
                        manifest = Some(m);
                        version = Some(v);
                        error = None;
                        stage = 41;
                    }
                    Err(e) => {
                        step!(message(term, "Version", &[e]));
                    }
                }
            }
            41 => {
                let items: Vec<(String, Option<String>)> = btrfs::Mode::ALL.iter().map(|m| (m.label().to_string(), None)).collect();
                let mut intro = vec!["What do you restore?".to_string()];
                if let Some(l) = bsrc.as_ref().and_then(|s| s.legacy.as_ref()) {
                    intro.extend(l.notes.iter().map(|n| format!("note: {n}")));
                }
                intro.extend(err_lines(&error));
                let i = step!(choose(term, "Mode", &intro, &items), stage = 40);
                mode = btrfs::Mode::ALL[i];
                error = None;
                if matches!(mode, btrfs::Mode::Custom | btrfs::Mode::Add) {
                    stage = 42;
                    continue;
                }
                let v = version.clone().ok_or(INTERNAL)?;
                let s = bsrc.as_mut().ok_or(INTERNAL)?;
                match s.choose(&v, mode, &[], &overrides, &|_| false) {
                    Ok(_) => {
                        stage = 5;
                    }
                    Err(e) => error = Some(e),
                }
            }
            42 => {
                let subs = layout(&manifest);
                let s = bsrc.as_ref().ok_or(INTERNAL)?;
                let whole = rynas_api_types::pcagent::BtrbkLayout { subvolumes: subs.clone(), ..Default::default() };
                let items: Vec<String> = subs
                    .iter()
                    .map(|x| {
                        let snap = overrides.iter().find(|(n, _)| n == &x.name).map(|(_, s)| s.clone()).unwrap_or_else(|| "the run's snapshot".into());
                        let place = if x.nested_in.is_empty() { None } else { whole.place_of(x) };
                        let at = btrfs::where_text(&x.mount, place.as_deref().unwrap_or(""));
                        format!("{}  {at}  ({snap})", x.name)
                    })
                    .collect();
                let mut intro = vec![if mode == btrfs::Mode::Add {
                    "Tick the subvolumes to add (a subvolume mounted or nested inside another needs that one, here or already in the PC's filesystem).".to_string()
                } else {
                    "Tick the subvolumes to restore (the root is needed; a subvolume mounted or nested inside another needs that one too).".to_string()
                }];
                intro.push(format!("{} snapshots on the NAS", s.listing.snapshots.len()));
                intro.extend(err_lines(&error));
                checked.resize(items.len(), true);
                match step!(checklist(term, "Subvolumes", &intro, &items, &mut checked), stage = 41) {
                    Some(i) => {
                        override_for = Some(i);
                        stage = 43;
                    }
                    None => {
                        let names: Vec<String> = subs.iter().zip(&checked).filter(|(_, c)| **c).map(|(x, _)| x.name.clone()).collect();
                        if mode == btrfs::Mode::Add {
                            if names.is_empty() {
                                error = Some("tick at least one subvolume".into());
                            } else {
                                error = None;
                                stage = 44;
                            }
                            continue;
                        }
                        let v = version.clone().ok_or(INTERNAL)?;
                        let s = bsrc.as_mut().ok_or(INTERNAL)?;
                        match s.choose(&v, mode, &names, &overrides, &|_| false) {
                            Ok(_) => {
                                error = None;
                                stage = 5;
                            }
                            Err(e) => error = Some(e),
                        }
                    }
                }
            }
            43 => {
                let subs = layout(&manifest);
                let i = override_for.ok_or(INTERNAL)?;
                let name = subs.get(i).map(|x| x.name.clone()).ok_or(INTERNAL)?;
                let s = bsrc.as_ref().ok_or(INTERNAL)?;
                let snaps: Vec<&rynas_api_types::pcbackup::ReceivedSnapshot> = s.listing.snapshots.iter().filter(|x| x.group == name && btrfs::complete(x)).collect();
                let mut items: Vec<(String, Option<String>)> = vec![("the run's snapshot".to_string(), None)];
                items.extend(snaps.iter().map(|x| (format!("{}  {}", x.name, x.size.map(|z| gib(z.referenced_bytes)).unwrap_or_default()), None)));
                let k = step!(choose(term, "Snapshot", &[format!("Which snapshot of {name}?")], &items), stage = 42);
                overrides.retain(|(n, _)| n != &name);
                if k > 0 {
                    overrides.push((name, snaps[k - 1].name.clone()));
                }
                stage = 42;
            }
            44 => {
                let devs = btrfs_devices();
                let items: Vec<(String, Option<String>)> = devs.iter().map(|d| (d.clone(), exec::disk_in_use(d.trim_start_matches("/dev/")))).collect();
                if items.is_empty() {
                    step!(message(term, "Add subvolumes", &["No btrfs filesystem found on this machine (blkid). Unlock or attach the PC's disk first.".to_string()]), stage = 42);
                    stage = 42;
                    continue;
                }
                let mut intro = vec!["The PC's btrfs filesystem to add to (it must not be mounted):".to_string()];
                intro.extend(err_lines(&error));
                let i = step!(choose(term, "Add to", &intro, &items), stage = 42);
                let device = devs[i].clone();
                let subs = layout(&manifest);
                let names: Vec<String> = subs.iter().zip(&checked).filter(|(_, c)| **c).map(|(x, _)| x.name.clone()).collect();
                let v = version.clone().ok_or(INTERNAL)?;
                let s = bsrc.as_mut().ok_or(INTERNAL)?;
                let restored = match s.prepare_add(&v, &device, &names, &overrides, false) {
                    Ok(r) => r,
                    Err(e) => {
                        error = Some(e);
                        continue;
                    }
                };
                let mut lines = s.describe();
                let (problems, notes) = super::btrfs_preflight(manifest.as_ref().ok_or(INTERNAL)?, s, true);
                lines.extend(notes.into_iter().map(|n| format!("note: {n}")));
                if !problems.is_empty() {
                    step!(message(term, "Before restoring", &problems), stage = 42);
                    continue;
                }
                let name = device.trim_start_matches("/dev/").to_string();
                lines.push(String::new());
                if dry {
                    lines.push("Dry run: nothing was written. Press Enter to exit.".into());
                    step!(message(term, "Plan", &lines), stage = 42);
                    return Ok(None);
                }
                lines.push(format!("The subvolumes are added to {device}; nothing is formatted. Type its name ({name}) to start:"));
                let typed = step!(input(term, "Plan", &lines, "", false), stage = 42);
                if typed.trim_start_matches("/dev/") != name {
                    error = Some("that is not the device's name".into());
                    continue;
                }
                return Ok(Some(Job::Add { src: Box::new(bsrc.take().ok_or(INTERNAL)?), v, m: manifest.take().ok_or(INTERNAL)?, device, restored }));
            }
            5 => {
                let base = manifest.as_ref().ok_or(INTERNAL)?;
                let src_index = super::source_disk(base).unwrap_or(0);
                let src_disk = base.disks.get(src_index);
                let mut intro = vec![match src_disk {
                    Some(d) if d.name == super::legacy::GUIDED => "The PC's layout is the guided one (an ESP and btrfs). Choose the target disk: it will be ERASED.".to_string(),
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
                let back = if use_btrfs { 41 } else { 4 };
                let i = step!(choose(term, "Target disk", &intro, &items), stage = back);
                let t = disks[i].clone();
                // btrfs: the chosen snapshots' sizes, a guided layout fitted to the disk
                let m = match bsrc.as_ref().filter(|_| use_btrfs) {
                    Some(s) => s.planned_manifest(base, t.logical_sector),
                    None => base.clone(),
                };
                let names = vec![t.name.clone()];
                let built = plan::build(&m, &[(src_index, t.clone())], &super::in_use_except(&names));
                match built {
                    Ok(p) => {
                        let mut problems = super::preflight(&m, &p);
                        if let Some(s) = bsrc.as_ref().filter(|_| use_btrfs) {
                            problems.extend(super::btrfs_preflight(&m, s, false).0);
                        }
                        if !problems.is_empty() {
                            step!(message(term, "Before restoring", &problems));
                            continue;
                        }
                        planned = Some(m);
                        target = Some(t);
                        the_plan = Some(p);
                        error = None;
                        stage = 6;
                    }
                    Err(e) => error = Some(e),
                }
            }
            6 => {
                let p = the_plan.as_ref().ok_or(INTERNAL)?;
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
                let (m, p, t) = (planned.as_ref().ok_or(INTERNAL)?, the_plan.as_ref().ok_or(INTERNAL)?, target.as_ref().ok_or(INTERNAL)?);
                let mut lines = plan::describe(m, p, use_btrfs);
                let skipped: Vec<String> = match bsrc.as_ref().filter(|_| use_btrfs) {
                    Some(s) => {
                        lines.extend(s.describe());
                        let (_, notes) = super::btrfs_preflight(m, s, false);
                        lines.extend(notes.into_iter().map(|n| format!("note: {n}")));
                        s.skipped_subvolumes()
                    }
                    None => Vec::new(),
                };
                lines.extend(m.ready_reasons().into_iter().map(|r| format!("note: {}", ready_text(r))));
                let back = if p.luks.is_empty() { 5 } else { 6 };
                if dry {
                    lines.push(String::new());
                    lines.extend(plan::dry_run_rewrites(m, &skipped));
                    lines.push("Dry run: nothing was written. Press Enter to exit.".into());
                    step!(message(term, "Plan", &lines), stage = back);
                    return Ok(None);
                }
                lines.push(String::new());
                lines.push(format!("/dev/{} ({} {}) WILL BE ERASED. Type its name ({}) to start:", t.name, t.model, t.serial, t.name));
                lines.extend(err_lines(&error));
                let typed = step!(input(term, "Plan", &lines, "", false), stage = back);
                if typed.trim_start_matches("/dev/") != t.name {
                    error = Some("that is not the disk's name".into());
                    continue;
                }
                let src: Box<dyn RestoreSource> = if use_btrfs { Box::new(bsrc.take().ok_or(INTERNAL)?) } else { Box::new(restic_src.take().ok_or(INTERNAL)?) };
                return Ok(Some(Job::Format { src, v: version.take().ok_or(INTERNAL)?, m: planned.take().ok_or(INTERNAL)?, p: the_plan.take().ok_or(INTERNAL)?, pass: pass.take() }));
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
