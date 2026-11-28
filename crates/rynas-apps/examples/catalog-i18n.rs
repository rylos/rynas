//! Stamps the translations of catalogue entries with the English the entries have now
//! (`[sources]` of `i18n/<lang>.toml`, `docs/phase2/docker-apps.md` § 6.14). Run it after
//! reviewing the translations of a reworded English text; the catalogue check refuses an entry
//! whose stamps no longer match.
//!
//! ```text
//! cargo run -p rynas-apps --example catalog-i18n -- apps/<id> [apps/<id>…]
//! cargo run -p rynas-apps --example catalog-i18n -- apps        # every entry
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args.is_empty() {
        eprintln!("usage: catalog-i18n apps/<id>… | apps");
        return ExitCode::from(2);
    }
    let mut entries = Vec::new();
    for a in args {
        if a.join("app.toml").exists() {
            entries.push(a);
            continue;
        }
        match std::fs::read_dir(&a) {
            Ok(rd) => {
                let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.join("app.toml").exists()).collect();
                v.sort();
                entries.extend(v);
            }
            Err(e) => {
                eprintln!("{}: {e}", a.display());
                return ExitCode::FAILURE;
            }
        }
    }
    let mut failed = false;
    for e in &entries {
        match rynas_apps::i18n::restamp(e) {
            Ok(files) => files.iter().for_each(|f| eprintln!("stamped {}", f.display())),
            Err(err) => {
                eprintln!("{}: {err}", e.display());
                failed = true;
            }
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
