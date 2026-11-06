//! Renders a catalogue entry with its test values and prints the stack files:
//! `cargo run -p rynas-apps --example render -- <apps dir> <id> [<output folder>]`
//! (with an output folder, the stack files are written there instead).

use rynas_apps::Catalog;
use rynas_apps::render::{self, Provided};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(dir), Some(id)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: render <apps dir> <id> [<output folder>]");
        std::process::exit(2);
    };
    let catalog = Catalog::load(std::path::Path::new(dir), "rynas").unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let Some(app) = catalog.get(id) else {
        eprintln!("no entry {id}");
        std::process::exit(1);
    };
    match render::render(app, &catalog.source, &Provided::example(id), &app.test_request()) {
        Ok(r) if args.len() > 3 => {
            // write the stack folder (packaging/apps-render-test.sh)
            let out = std::path::Path::new(&args[3]);
            std::fs::create_dir_all(out).expect("create output folder");
            for f in &r.files {
                std::fs::write(out.join(&f.path), &f.contents).expect("write stack file");
            }
        }
        Ok(r) => {
            for f in &r.files {
                println!("==> {} (mode {:o})\n{}", f.path, f.mode, String::from_utf8_lossy(&f.contents));
            }
            for f in &r.folders {
                println!("folder {} -> {} owner {:?} share {}", f.env, f.host_path, f.owner, f.share);
            }
        }
        Err(e) => {
            eprintln!("{}: {e}", e.code());
            std::process::exit(1);
        }
    }
}
