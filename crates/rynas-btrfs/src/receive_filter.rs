//! The forced command of every PC key (`rynas-btrbk-filter`): an allowlist of the commands
//! btrbk sends to a target, written in Rust instead of btrbk's `ssh_filter_btrbk.sh`.
//!
//! Why our own: the shell filter matches the command with regular expressions and then runs
//! it through the shell; versions 0.29.0–0.32.6 (Debian trixie ships 0.32.6) had no end
//! anchor and let a `| any command` through (CVE-2026-62943). Here the command is split into
//! words by a tiny tokenizer (plain words, single-quoted words, `|`), every stage is matched
//! word by word, and the result is run **without a shell**, with absolute program paths and
//! a clean environment.
//!
//! What a key may do (the shapes btrbk 0.32 sends, with or without `sudo -n`/`doas -n`):
//! - `btrfs receive [--max-errors=0] '<root>[/…]'`, behind optional `<zstd|…> -d -c` and
//!   `mbuffer -v 1 -q …` stages; run as `btrfs receive --chroot` (confined to the folder);
//! - with `--delete`: `btrfs subvolume delete [--commit-each|--commit-after] '<root>/<name>'…`
//!   for btrbk snapshot names directly inside the folder, subject to the NAS rules
//!   ([`crate::receive::delete_refusal`], checked by the binary on a fresh listing);
//! - read only: `btrfs subvolume show|list …`, `btrfs filesystem usage …`,
//!   `readlink [-v] -e|-f '<root>…'`, `test -d '<root>…'`, `cat /proc/self/mountinfo|mounts`.
//!
//! Everything else (other programs, `;`, `&`, `$`, backquotes, redirections, double quotes,
//! `..`, paths outside the folder) is refused. btrfs runs through `sudo -n` when the filter
//! is not root (the sudoers file allows only those btrfs subcommands).

use std::path::{Path, PathBuf};

/// The filter's arguments, written in the key line by rynasd.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterArgs {
    pub device: String,
    pub root: String,
    pub delete: bool,
    pub keep_last: u32,
    pub min_age_days: u32,
}

pub fn parse_args(args: &[String]) -> Result<FilterArgs, String> {
    let mut a = FilterArgs::default();
    let mut it = args.iter();
    let num = |v: Option<&String>, what: &str| {
        v.and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| format!("{what} needs a number"))
    };
    while let Some(k) = it.next() {
        match k.as_str() {
            "--device" => a.device = it.next().cloned().ok_or("--device needs a name")?,
            "--root" => a.root = it.next().cloned().ok_or("--root needs a path")?,
            "--delete" => a.delete = true,
            "--keep-last" => a.keep_last = num(it.next(), "--keep-last")?,
            "--min-age-days" => a.min_age_days = num(it.next(), "--min-age-days")?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.device.is_empty() || crate::receive::valid_path(&a.root).is_err() {
        return Err("--device and a valid --root are needed".into());
    }
    Ok(a)
}

/// A program the filter may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prog {
    /// Run as root (through `sudo -n` when the filter is not root).
    Btrfs,
    Readlink,
    Test,
    Cat,
    /// A decompressor in front of `btrfs receive`.
    Decompress(&'static str),
    Mbuffer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub prog: Prog,
    pub args: Vec<String>,
}

/// An accepted command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub stages: Vec<Stage>,
    /// Snapshot names a delete asks for (the binary checks the NAS rules on them).
    pub deletes: Vec<String>,
    /// One line for the log.
    pub summary: String,
}

const DECOMPRESS: &[&str] = &["gzip", "pigz", "bzip2", "pbzip2", "bzip3", "xz", "lzop", "lz4", "zstd"];
const MAX_LEN: usize = 16 * 1024;

/// Words of each pipeline stage. Plain words take `[A-Za-z0-9_@+./=,:%-]`; single quotes
/// take anything but a quote, a newline or NUL; `|` separates stages.
pub fn tokenize(cmd: &str) -> Result<Vec<Vec<String>>, String> {
    if cmd.len() > MAX_LEN {
        return Err("command too long".into());
    }
    let mut stages = vec![Vec::new()];
    let mut word: Option<String> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if let Some(w) = word.take() {
                    stages.last_mut().expect("one stage").push(w);
                }
            }
            '|' => {
                if let Some(w) = word.take() {
                    stages.last_mut().expect("one stage").push(w);
                }
                stages.push(Vec::new());
            }
            '\'' => {
                let w = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\n' | '\r' | '\0') | None => {
                            return Err("unterminated or invalid quoted word".into());
                        }
                        Some(q) => w.push(q),
                    }
                }
            }
            c if c.is_ascii_alphanumeric() || "_@+./=,:%-".contains(c) => {
                word.get_or_insert_default().push(c);
            }
            other => return Err(format!("character {other:?} not allowed")),
        }
    }
    if let Some(w) = word.take() {
        stages.last_mut().expect("one stage").push(w);
    }
    if stages.iter().any(Vec::is_empty) {
        return Err("empty command or pipeline stage".into());
    }
    Ok(stages)
}

/// `p` as a normalized absolute path: no `.`/`..`/empty parts, no trailing `/` (but `/`).
fn normal(p: &str) -> Option<String> {
    let t = p.trim_end_matches('/');
    if !p.starts_with('/') || p.contains('\0') {
        return None;
    }
    if t.is_empty() {
        return Some("/".into());
    }
    let ok = t[1..].split('/').all(|s| !s.is_empty() && s != "." && s != "..");
    ok.then(|| t.to_string())
}

fn under(p: &str, root: &str) -> Option<String> {
    let n = normal(p)?;
    (n == root || n.starts_with(&format!("{root}/"))).then_some(n)
}

fn is_num(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `N` with an optional unit, as mbuffer takes it.
fn is_size(s: &str) -> bool {
    let t = s.trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G', 't', 'T', 'b', '%']);
    is_num(t) && s.len() - t.len() <= 1
}

/// Checks one command against the device's arguments.
pub fn check(cmd: &str, a: &FilterArgs) -> Result<Plan, String> {
    let stages = tokenize(cmd)?;
    let n = stages.len();
    let mut plan = Plan::default();
    for (i, words) in stages.into_iter().enumerate() {
        let last = i + 1 == n;
        let mut w: &[String] = &words;
        if w.first().is_some_and(|x| x == "sudo" || x == "doas") {
            if w.get(1).map(String::as_str) != Some("-n") {
                return Err("sudo/doas only with -n".into());
            }
            w = &w[2..];
        }
        let (prog, rest) = w.split_first().ok_or("empty stage")?;
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        let stage = if last {
            final_stage(prog, &rest, a, n > 1, &mut plan)?
        } else {
            stream_stage(prog, &rest)?
        };
        plan.stages.push(stage);
    }
    plan.summary = cmd.chars().take(300).collect();
    Ok(plan)
}

/// A decompressor or mbuffer in front of `btrfs receive`.
fn stream_stage(prog: &str, rest: &[&str]) -> Result<Stage, String> {
    let args: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
    if let Some(d) = DECOMPRESS.iter().find(|d| **d == prog) {
        let ok = rest.len() >= 2
            && rest[0] == "-d"
            && rest[1] == "-c"
            && rest[2..].iter().all(|x| {
                x.strip_prefix("-p").or_else(|| x.strip_prefix("-T")).is_some_and(is_num)
            })
            && rest.len() <= 3;
        return if ok {
            Ok(Stage { prog: Prog::Decompress(d), args })
        } else {
            Err(format!("{prog}: only -d -c [-pN|-TN]"))
        };
    }
    if prog == "mbuffer" {
        let mut ok = rest.len() >= 3 && rest[..3] == ["-v", "1", "-q"];
        let mut i = 3;
        while ok && i < rest.len() {
            ok = matches!(rest[i], "-s" | "-m" | "-r" | "-R")
                && rest.get(i + 1).is_some_and(|v| is_size(v));
            i += 2;
        }
        return if ok {
            Ok(Stage { prog: Prog::Mbuffer, args })
        } else {
            Err("mbuffer: only -v 1 -q [-s|-m|-r|-R N]".into())
        };
    }
    Err(format!("{prog}: not allowed in front of btrfs receive"))
}

fn final_stage(
    prog: &str,
    rest: &[&str],
    a: &FilterArgs,
    piped: bool,
    plan: &mut Plan,
) -> Result<Stage, String> {
    let root = a.root.as_str();
    let s = |prog: Prog, args: Vec<String>| Ok(Stage { prog, args });
    match (prog, rest) {
        ("btrfs", ["receive", more @ ..]) => {
            let (opts, path) = more.split_at(more.len().saturating_sub(1));
            let path = path.first().ok_or("btrfs receive: no folder")?;
            let dest = under(path, root).ok_or("btrfs receive: outside this device's folder")?;
            let mut args = vec!["receive".to_string()];
            for o in opts {
                match *o {
                    "--max-errors=0" | "-v" | "-q" => args.push(o.to_string()),
                    _ => return Err(format!("btrfs receive: option {o} not allowed")),
                }
            }
            args.extend(["--chroot".to_string(), format!("{dest}/")]);
            s(Prog::Btrfs, args)
        }
        _ if piped => Err("only btrfs receive takes a stream".into()),
        ("btrfs", ["subvolume", "delete", more @ ..]) => {
            if !a.delete {
                return Err("deleting is turned off on the NAS for this device".into());
            }
            let mut args = vec!["subvolume".to_string(), "delete".to_string()];
            let mut any = false;
            for o in more {
                match *o {
                    "--commit-each" | "--commit-after" | "-c" | "-C" if !any => {
                        args.push(o.to_string())
                    }
                    p => {
                        let n = normal(p).ok_or("btrfs subvolume delete: bad path")?;
                        let (parent, name) = n.rsplit_once('/').ok_or("bad path")?;
                        if parent != root
                            || crate::name::split_snapshot_name(name).is_none()
                        {
                            return Err(format!(
                                "btrfs subvolume delete: {p} is not a snapshot of this device"
                            ));
                        }
                        plan.deletes.push(name.to_string());
                        args.push(n);
                        any = true;
                    }
                }
            }
            if !any {
                return Err("btrfs subvolume delete: no subvolume".into());
            }
            s(Prog::Btrfs, args)
        }
        ("btrfs", ["subvolume", sub @ ("show" | "list"), more @ ..]) => {
            let (opts, path) = more.split_at(more.len().saturating_sub(1));
            let path = path.first().ok_or("btrfs subvolume: no path")?;
            let p = normal(path).ok_or("btrfs subvolume: bad path")?;
            if opts
                .iter()
                .any(|o| !(o.len() >= 2 && o.starts_with('-') && o[1..].bytes().all(|b| b.is_ascii_alphabetic())))
            {
                return Err("btrfs subvolume: option not allowed".into());
            }
            let mut args = vec!["subvolume".to_string(), sub.to_string()];
            args.extend(opts.iter().map(|o| o.to_string()));
            args.push(p);
            s(Prog::Btrfs, args)
        }
        ("btrfs", ["filesystem", "usage", more @ ..]) => {
            let (opts, path) = more.split_at(more.len().saturating_sub(1));
            let path = path.first().ok_or("btrfs filesystem usage: no path")?;
            let p = normal(path).ok_or("btrfs filesystem usage: bad path")?;
            if opts.iter().any(|o| !matches!(*o, "-b" | "--raw" | "-T")) {
                return Err("btrfs filesystem usage: option not allowed".into());
            }
            let mut args = vec!["filesystem".to_string(), "usage".to_string()];
            args.extend(opts.iter().map(|o| o.to_string()));
            args.push(p);
            s(Prog::Btrfs, args)
        }
        ("readlink", more) if !more.is_empty() => {
            let (opts, path) = more.split_at(more.len() - 1);
            if opts.iter().any(|o| !matches!(*o, "-v" | "-e" | "-f")) {
                return Err("readlink: only -v -e -f".into());
            }
            let p = under(path[0], root).ok_or("readlink: outside this device's folder")?;
            let mut args: Vec<String> = opts.iter().map(|o| o.to_string()).collect();
            args.push(p);
            s(Prog::Readlink, args)
        }
        ("test", ["-d", path]) => {
            let p = under(path, root).ok_or("test: outside this device's folder")?;
            s(Prog::Test, vec!["-d".into(), p])
        }
        ("cat", [f @ ("/proc/self/mountinfo" | "/proc/self/mounts")]) => {
            s(Prog::Cat, vec![f.to_string()])
        }
        _ => Err(format!("{prog}: not allowed for this key")),
    }
}

/// Where a program is: the first of the usual places that exists.
pub fn locate(prog: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|d| Path::new(d).join(prog))
        .find(|p| p.is_file())
}

impl Prog {
    pub fn name(self) -> &'static str {
        match self {
            Prog::Btrfs => "btrfs",
            Prog::Readlink => "readlink",
            Prog::Test => "test",
            Prog::Cat => "cat",
            Prog::Decompress(d) => d,
            Prog::Mbuffer => "mbuffer",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(delete: bool) -> FilterArgs {
        FilterArgs { device: "pc".into(), root: "/volume1/backup/pc".into(), delete, keep_last: 0, min_age_days: 0 }
    }

    #[test]
    fn btrbk_commands_pass() {
        let a = args(true);
        let ok = |c: &str| check(c, &a).unwrap_or_else(|e| panic!("{c}: {e}"));
        let p = ok("sudo -n btrfs receive '/volume1/backup/pc/'");
        assert_eq!(p.stages[0].args, ["receive", "--chroot", "/volume1/backup/pc/"]);
        let p = ok("zstd -d -c | mbuffer -v 1 -q -s 128k -m 2G | sudo -n btrfs receive --max-errors=0 '/volume1/backup/pc/'");
        assert_eq!(p.stages.len(), 3);
        assert_eq!(p.stages[0].prog, Prog::Decompress("zstd"));
        assert_eq!(p.stages[2].args, ["receive", "--max-errors=0", "--chroot", "/volume1/backup/pc/"]);
        let p = ok("sudo -n btrfs subvolume delete --commit-each '/volume1/backup/pc/home.20261108T0130'");
        assert_eq!(p.deletes, ["home.20261108T0130"]);
        assert_eq!(p.stages[0].args, ["subvolume", "delete", "--commit-each", "/volume1/backup/pc/home.20261108T0130"]);
        ok("sudo -n btrfs subvolume list -a -c -u -q -R '/volume1'");
        ok("btrfs subvolume show '/volume1/backup/pc'");
        ok("sudo -n readlink -v -e '/volume1/backup/pc'");
        ok("readlink -f /volume1/backup/pc");
        ok("test -d '/volume1/backup/pc'");
        ok("cat /proc/self/mountinfo");
        ok("btrfs filesystem usage -b '/volume1/backup/pc'");
        ok("doas -n btrfs receive /volume1/backup/pc/sub");
    }

    #[test]
    fn everything_else_is_refused() {
        let a = args(false);
        for c in [
            "",
            "sh",
            "btrfs receive '/volume1/backup/pc/' | sh",
            "btrfs receive '/volume1/backup/pc/'; rm -rf /",
            "btrfs receive '/volume1/backup/pc/' && id",
            "btrfs receive \"/volume1/backup/pc/\"",
            "btrfs receive '/volume1/backup/other/'",
            "btrfs receive '/volume1/backup/pc/../other'",
            "btrfs receive '/volume1/backup/pcx/'",
            "btrfs receive -e '/volume1/backup/pc/'",
            "btrfs receive $(id)",
            "btrfs receive `id`",
            "btrfs receive '/volume1/backup/pc/' > /etc/passwd",
            "btrfs receive '/volume1/backup/pc/\n'",
            "btrfs send /volume1/backup/pc/x",
            "btrfs subvolume snapshot /a /b",
            "btrfs subvolume delete '/volume1/backup/pc/home.20261108T0130'",
            "sudo btrfs receive '/volume1/backup/pc/'",
            "sudo -n sh -c id",
            "zstd -d -c | sh",
            "zstd -d -c -o /etc/x | btrfs receive '/volume1/backup/pc/'",
            "mbuffer -v 1 -q -o /etc/x | btrfs receive '/volume1/backup/pc/'",
            "zstd -d -c | btrfs subvolume list /",
            "cat /etc/shadow",
            "readlink -e /etc",
            "test -f '/volume1/backup/pc'",
            "mkdir -p '/volume1/backup/pc/x'",
            "btrfs subvolume list --sort=x /",
            "btrfs receive",
        ] {
            assert!(check(c, &a).is_err(), "accepted: {c:?}");
        }
        // with --delete: only snapshot names directly inside the folder
        let a = args(true);
        for c in [
            "btrfs subvolume delete '/volume1/backup/pc'",
            "btrfs subvolume delete '/volume1/backup/pc/sub/home.20261108T0130'",
            "btrfs subvolume delete '/volume1/backup/pc/notasnapshot'",
            "btrfs subvolume delete '/volume1/backup/other/home.20261108T0130'",
            "btrfs subvolume delete --commit-each",
        ] {
            assert!(check(c, &a).is_err(), "accepted: {c:?}");
        }
    }

    #[test]
    fn arguments_round_trip() {
        let p = rynas_api_types::pcbackup::DeletePolicy { allowed: true, keep_last: 3, min_age_days: 7 };
        let a = parse_args(&crate::receive::filter_args("pc", "/volume1/backup/pc", &p)).unwrap();
        assert_eq!(a, FilterArgs { device: "pc".into(), root: "/volume1/backup/pc".into(), delete: true, keep_last: 3, min_age_days: 7 });
        assert!(parse_args(&["--device".into(), "pc".into()]).is_err());
        assert!(parse_args(&["--device".into(), "pc".into(), "--root".into(), "/etc/x".into()]).is_err());
        assert!(parse_args(&["--bogus".into()]).is_err());
    }
}
