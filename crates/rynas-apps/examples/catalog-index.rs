//! Writes the `index.json` of an online catalogue (packaging/publish-catalog.sh):
//! `cargo run -p rynas-apps --features online --example catalog-index -- <apps dir> <serial>
//! <published> <expires> <archive url> <archive file>`.
//!
//! The catalogue must load without a single issue and its `catalog.toml` must already carry
//! `<serial>`; the archive's size and SHA-256 are read from the file. The index goes to
//! standard output.

use rynas_apps::Catalog;
use rynas_apps::catalog::read_serial;
use rynas_apps::online::{ArchiveRef, INDEX_SCHEMA, Index, IndexApp, sha256_hex};

fn fail(m: impl std::fmt::Display) -> ! {
    eprintln!("catalog-index: {m}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, serial, published, expires, url, file] = args.as_slice() else {
        fail("usage: catalog-index <apps dir> <serial> <published> <expires> <archive url> <archive file>");
    };
    let dir = std::path::Path::new(dir);
    let serial: u64 = serial.parse().unwrap_or_else(|_| fail("serial is not a number"));
    match read_serial(dir) {
        Ok(Some(s)) if s == serial => {}
        Ok(s) => fail(format!("catalog.toml has serial {s:?}, expected {serial}")),
        Err(e) => fail(e),
    }
    let catalog = Catalog::load(dir, "rynas").unwrap_or_else(|e| fail(format!("the catalogue does not validate:\n{e}")));
    let data = std::fs::read(file).unwrap_or_else(|e| fail(format!("{file}: {e}")));
    let index = Index {
        schema: INDEX_SCHEMA,
        serial,
        published: Some(published.clone()),
        expires: expires.clone(),
        archive: ArchiveRef { url: url.clone(), sha256: sha256_hex(&data), size: data.len() as u64 },
        apps: catalog
            .apps
            .values()
            .map(|a| IndexApp { id: a.manifest.id.clone(), version: a.manifest.version, min_rynas: a.manifest.min_rynas })
            .collect(),
    };
    if let Err(e) = index.expires_at() {
        fail(e);
    }
    println!("{}", serde_json::to_string_pretty(&index).unwrap_or_else(|e| fail(e)));
}
