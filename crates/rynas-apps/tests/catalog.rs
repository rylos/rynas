//! CI lint of the curated catalogue (`apps/` at the repository root): every entry loads and
//! validates (schema, form, compose structure for every overlay variant, metadata), renders
//! with its `tests/values.toml`, and the rendered stack is an ordinary compose project.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rynas_apps::compose::{self, ImageRef};
use rynas_apps::plan::{self, ImageChangeKind, RamGate, UsedPort};
use rynas_apps::render::{self, Provided};
use rynas_apps::{Catalog, CatalogApp, RenderError};
use yaml_serde::Value;

fn apps_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps")
}

fn catalog() -> Catalog {
    match Catalog::load(&apps_dir(), "rynas") {
        Ok(c) => c,
        Err(issues) => panic!("catalogue does not validate:\n{issues}"),
    }
}

#[test]
fn catalogue_validates_and_has_the_wave_a1_entries() {
    let c = catalog();
    for id in [
        "arcane", "dockhand", "portainer-ce", "dockge", "immich", "jellyfin", "vaultwarden", "nextcloud", "syncthing", "home-assistant",
        "uptime-kuma", "paperless-ngx",
    ] {
        assert!(c.get(id).is_some(), "missing entry {id}");
    }
}

#[test]
fn catalogue_has_the_wave_a3_entries_and_a_featured_pick() {
    let c = catalog();
    for id in [
        "nginx-proxy-manager", "pocket-id", "cloudflared", "adguard-home", "navidrome", "audiobookshelf", "stirling-pdf", "actual-budget",
        "mosquitto", "zigbee2mqtt", "frigate", "forgejo", "mealie",
    ] {
        assert!(c.get(id).is_some(), "missing entry {id}");
    }
    // + JDownloader (2026-11-10)
    assert_eq!(c.apps.len(), 27);
    let featured: Vec<&str> = c.apps.values().filter(|a| a.manifest.featured).map(|a| a.manifest.id.as_str()).collect();
    assert!((5..=10).contains(&featured.len()), "featured is a small pick: {featured:?}");
    assert!(featured.contains(&"immich"));
    // only the reverse proxies default to 80/443
    let proxies: Vec<&str> = c.apps.values().filter(|a| a.manifest.reverse_proxy).map(|a| a.manifest.id.as_str()).collect();
    assert_eq!(proxies, ["nginx-proxy-manager"]);
}

#[test]
fn device_overlays_render() {
    let c = catalog();
    let p = Provided::example("x");
    let z2m = c.get("zigbee2mqtt").unwrap();
    let r = render::render(z2m, "rynas", &p, &z2m.test_request()).unwrap();
    assert_eq!(r.overlays, ["overlays/usb-adapter.yaml"]);
    let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
    assert!(text.contains("${ADAPTER_DEVICE}:/dev/zigbee"), "{text}");
    assert_eq!(r.env["ADAPTER_DEVICE"], "/dev/serial/by-id/usb-Example_Zigbee_Adapter-if00-port0");

    let frigate = c.get("frigate").unwrap();
    let r = render::render(frigate, "rynas", &p, &frigate.test_request()).unwrap();
    let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
    assert!(text.contains("type: openvino") && text.contains("/dev/dri:/dev/dri"));
    assert_eq!(r.env["LIBVA_DRIVER"], "i965");
    // without the GPU the seeded config keeps the default CPU detector
    let mut req = frigate.test_request();
    req.form.insert("HW_ACCEL".into(), serde_json::Value::Bool(false));
    let r = render::render(frigate, "rynas", &p, &req).unwrap();
    let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
    assert!(!text.contains("openvino") && !text.contains("/dev/dri"));
    // the recordings folder asks for no copy-on-write; the config folder (SQLite, small) not
    let nocow: Vec<(&str, bool)> = r.folders.iter().map(|f| (f.env.as_str(), f.nocow)).collect();
    assert!(nocow.contains(&("RECORDINGS_FOLDER", true)) && nocow.contains(&("CONFIG_FOLDER", false)), "{nocow:?}");
}

/// `reverse_proxy` owns 80/443; `security.devices` gates non-GPU devices.
#[test]
fn lint_checks_reverse_proxies_and_devices() {
    let tmp = tempfile::tempdir().unwrap();
    // an ordinary app may not default to 80
    let dst = tmp.path().join("navidrome");
    copy_dir(&apps_dir().join("navidrome"), &dst);
    let t = fs::read_to_string(dst.join("app.toml")).unwrap().replace("default = 4533", "default = 80");
    fs::write(dst.join("app.toml"), t).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    assert!(text.contains("belongs to the web access proxy"), "{text}");
    // nor a host-network app (review 2026-11-07, L5)
    let dst = tmp.path().join("home-assistant");
    copy_dir(&apps_dir().join("home-assistant"), &dst);
    let t = fs::read_to_string(dst.join("app.toml"))
        .unwrap()
        .replace("container = 8123\ndefault = 8123", "container = 80\ndefault = 80");
    fs::write(dst.join("app.toml"), t).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    assert!(text.contains("belongs to the web access proxy"), "{text}");

    // a reverse proxy's 80/443 are fixed
    let dst = tmp.path().join("nginx-proxy-manager");
    copy_dir(&apps_dir().join("nginx-proxy-manager"), &dst);
    let t = fs::read_to_string(dst.join("app.toml")).unwrap().replacen("relocatable = false\n", "", 1);
    fs::write(dst.join("app.toml"), t).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    assert!(text.contains("a reverse proxy's port 80 must be relocatable = false"), "{text}");

    // without security.devices the USB overlay is refused
    let dst = tmp.path().join("zigbee2mqtt");
    copy_dir(&apps_dir().join("zigbee2mqtt"), &dst);
    let t = fs::read_to_string(dst.join("app.toml")).unwrap().replace("devices = [\"zigbee2mqtt\"]", "");
    fs::write(dst.join("app.toml"), t).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    assert!(text.contains("devices need hardware.gpu or security.devices"), "{text}");
}

fn check_rendered(app: &CatalogApp, rendered: &render::RenderedStack) {
    let id = &app.manifest.id;
    let compose_file = rendered.file("compose.yaml").expect("compose.yaml");
    let env_file = rendered.file(".env").expect(".env");
    assert_eq!(compose_file.mode, 0o644);
    assert_eq!(env_file.mode, 0o600);
    let text = String::from_utf8(compose_file.contents.clone()).unwrap();
    let doc: Value = yaml_serde::from_str(&text).unwrap_or_else(|e| panic!("{id}: rendered compose does not parse: {e}"));
    assert_eq!(doc.get("name").and_then(Value::as_str), Some(rendered.stack.as_str()), "{id}: name");
    let meta = render::installed_meta(&text).unwrap().expect("x-rynas");
    assert_eq!(meta.app, *id);
    assert_eq!(meta.version, app.manifest.version);

    // only ${VAR} kept, each defined in .env
    let env = render::parse_env(&String::from_utf8(env_file.contents.clone()).unwrap());
    assert_eq!(env, rendered.env, "{id}: .env round trip");
    for r in compose::refs(&doc).unwrap() {
        assert!(r.plain, "{id}: non-plain reference {}", r.name);
        assert!(env.contains_key(&r.name), "{id}: ${{{}}} not in .env", r.name);
    }
    // every image pinned by tag and digest, never latest
    for (svc, img) in compose::images(&doc) {
        let r = ImageRef::parse(&img).unwrap();
        assert!(r.digest.is_some(), "{id}/{svc}: {img} not pinned");
        assert!(!matches!(r.tag.as_deref(), None | Some("latest")), "{id}/{svc}: {img} tag");
    }
    // secrets generated and kept out of the compose file
    for s in &app.manifest.secrets {
        let v = &env[&s.env];
        assert!(!v.is_empty() && !text.contains(v.as_str()), "{id}: secret {} leaked or empty", s.env);
    }
    // folders: stack-relative by default, absolute host paths for creation
    for f in &rendered.folders {
        assert!(f.host_path.starts_with('/'), "{id}: {} not absolute", f.host_path);
    }
    // "Convert to custom stack" drops x-rynas and nothing else
    let custom = render::convert_to_custom(&text).unwrap();
    assert_eq!(render::installed_meta(&custom).unwrap(), None);
    let custom_doc: Value = yaml_serde::from_str(&custom).unwrap();
    assert_eq!(custom_doc.get("services"), doc.get("services"));
}

#[test]
fn every_entry_renders_with_its_test_values_and_with_defaults() {
    let c = catalog();
    for app in c.apps.values() {
        let p = Provided::example(&app.manifest.id);
        let r = render::render(app, &c.source, &p, &app.test_request()).unwrap_or_else(|e| panic!("{}: {e}", app.manifest.id));
        check_rendered(app, &r);

        // the defaults (no overlay, no shares chosen) render too when the form allows it
        let mut req = app.test_request();
        req.shares.clear();
        req.folders.clear();
        req.ports.clear();
        for b in app.form.booleans() {
            req.form.insert(b.to_owned(), serde_json::Value::Bool(false));
        }
        let r = render::render(app, &c.source, &p, &req).unwrap();
        check_rendered(app, &r);
        for f in &app.manifest.folders {
            assert_eq!(r.env[&f.env], format!("./{}", f.path));
        }
    }
}

#[test]
fn overlays_are_applied_by_condition() {
    let c = catalog();
    let immich = c.get("immich").unwrap();
    let p = Provided::example("photos");
    let mut req = immich.test_request();
    req.stack = "photos".into();
    let r = render::render(immich, "rynas", &p, &req).unwrap();
    assert_eq!(r.overlays, ["overlays/no-ml.yaml", "overlays/hw-transcoding.yaml"]);
    assert!(!r.images.iter().any(|(s, _)| s == "immich-machine-learning"));
    let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
    assert!(text.contains("/dev/dri:/dev/dri"));
    assert_eq!(r.env["DB_DATA_LOCATION"], "/volume2/ssd/immich-db");
    assert_eq!(r.env["UPLOAD_LOCATION"], "./library");
    assert!(r.folders.iter().any(|f| f.share && f.host_path == "/volume1/photo"));
}

#[test]
fn proxied_ports_bind_to_rynas_bind() {
    let c = catalog();
    for app in c.apps.values() {
        let r = render::render(app, "rynas", &Provided::example("x"), &app.test_request()).unwrap();
        let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
        for p in &app.manifest.ports {
            let bound = format!("${{RYNAS_BIND}}:${{{}}}:{}", p.env, p.container);
            assert_eq!(text.contains(&bound), p.proxied, "{}: port {}", app.manifest.id, p.name);
        }
        assert_eq!(r.env["RYNAS_BIND"], "127.0.0.1");
    }
}

#[test]
fn render_errors_are_precise() {
    let c = catalog();
    let vw = c.get("vaultwarden").unwrap();
    let p = Provided::example("vaultwarden");
    let mut req = vw.test_request();
    req.form.remove("DOMAIN");
    assert_eq!(render::render(vw, "rynas", &p, &req), Err(RenderError::MissingValue("DOMAIN".into())));
    req.form.insert("DOMAIN".into(), serde_json::json!("http://vault.example.lan"));
    assert!(matches!(render::render(vw, "rynas", &p, &req), Err(RenderError::InvalidValue { .. })));
    let mut req = vw.test_request();
    req.stack = "Bad Name".into();
    assert!(matches!(render::render(vw, "rynas", &p, &req), Err(RenderError::InvalidStackName(_))));
    let mut req = vw.test_request();
    req.folders.insert("DATA_FOLDER".into(), "relative/path".into());
    assert!(matches!(render::render(vw, "rynas", &p, &req), Err(RenderError::InvalidValue { .. })));
    let mut req = vw.test_request();
    req.shares.insert("NOPE".into(), "/x".into());
    assert!(matches!(render::render(vw, "rynas", &p, &req), Err(RenderError::UnknownValue(_))));

    let ha = c.get("home-assistant").unwrap();
    let mut req = ha.test_request();
    req.ports.insert("WEB_PORT".into(), 9999);
    assert!(matches!(render::render(ha, "rynas", &p, &req), Err(RenderError::InvalidValue { .. })), "host network ports are fixed");
}

#[test]
fn port_conflicts_and_ram_gate() {
    let c = catalog();
    let st = c.get("syncthing").unwrap();
    let used = [UsedPort { port: 8384, protocol: rynas_apps::manifest::Protocol::Tcp }, UsedPort { port: 21027, protocol: rynas_apps::manifest::Protocol::Udp }];
    let conflicts = plan::port_conflicts(&st.manifest, &BTreeMap::new(), &used);
    assert_eq!(conflicts.len(), 2);
    assert_eq!((conflicts[0].name.as_str(), conflicts[0].suggestion), ("web", Some(8385)));
    assert_eq!((conflicts[1].name.as_str(), conflicts[1].relocatable), ("discovery", false));

    let immich = c.get("immich").unwrap();
    // a 4 GB NAS reports ~3.7 GiB of MemTotal
    assert!(matches!(plan::ram_gate(&immich.manifest, 3800), RamGate::BelowRecommended { .. }));
    assert!(matches!(plan::ram_gate(&immich.manifest, 2000), RamGate::BelowMinimum { .. }));
    assert_eq!(plan::ram_gate(&immich.manifest, 16000), RamGate::Ok);
}

/// Simulates an install of an older package version and plans the update.
#[test]
fn update_plan_lists_image_changes_and_migrations() {
    let tmp = tempfile::tempdir().unwrap();
    let src = apps_dir().join("uptime-kuma");
    let dst = tmp.path().join("uptime-kuma");
    copy_dir(&src, &dst);
    // the catalogue moves to 1.1.0 with a renamed value
    let toml_text = fs::read_to_string(dst.join("app.toml")).unwrap().replace("version = \"1.0.0\"", "version = \"1.1.0\"");
    fs::write(dst.join("app.toml"), toml_text).unwrap();
    fs::create_dir_all(dst.join("migrations")).unwrap();
    fs::write(
        dst.join("migrations/1.1.0.toml"),
        "to = \"1.1.0\"\ndescription = \"test\"\n[[ops]]\nop = \"set_default\"\nkey = \"EXTRA\"\nvalue = \"x\"\n",
    )
    .unwrap();
    let new = CatalogApp::load(&dst).unwrap_or_else(|e| panic!("{e}"));

    // installed: version 1.0.0 with an older digest
    let old = catalog();
    let old_app = old.get("uptime-kuma").unwrap();
    let mut req = old_app.test_request();
    req.ports.insert("WEB_PORT".into(), 3002);
    let installed = render::render(old_app, "rynas", &Provided::example("uptime-kuma"), &req).unwrap();
    let old_digest = "sha256:".to_owned() + &"0".repeat(64);
    let compose_text = String::from_utf8(installed.file("compose.yaml").unwrap().contents.clone()).unwrap();
    let new_digest = installed.images[0].1.digest.clone().unwrap();
    let compose_text = compose_text.replace(&new_digest, &old_digest);
    let env_text = String::from_utf8(installed.file(".env").unwrap().contents.clone()).unwrap();

    let plan = plan::update_plan(&new, &compose_text, &env_text).unwrap();
    assert_eq!(plan.from_version.to_string(), "1.0.0");
    assert_eq!(plan.to_version.to_string(), "1.1.0");
    assert!(!plan.up_to_date && !plan.downgrade);
    assert_eq!(plan.migrations.len(), 1);
    assert_eq!(plan.image_changes.len(), 1);
    assert_eq!(plan.image_changes[0].kind, ImageChangeKind::Digest);

    // the re-render keeps the moved port; the migrated extra value is unknown to the form and
    // stays out of the request
    let updated = plan::prepare_update(&new, "rynas", &Provided::example("uptime-kuma"), "uptime-kuma", &compose_text, &env_text, &BTreeMap::new()).unwrap();
    assert_eq!(updated.env["WEB_PORT"], "3002");
    assert_eq!(updated.meta.version.to_string(), "1.1.0");

    // the same stack against the same catalogue is up to date
    let same = plan::update_plan(old_app, &String::from_utf8(installed.file("compose.yaml").unwrap().contents.clone()).unwrap(), &env_text).unwrap();
    assert!(same.up_to_date, "{same:?}");
    // a custom stack has no plan
    let custom = render::convert_to_custom(&compose_text).unwrap();
    assert!(matches!(plan::update_plan(&new, &custom, &env_text), Err(RenderError::NotInstalledApp(_))));
}

#[test]
fn secrets_survive_updates() {
    let c = catalog();
    let pl = c.get("paperless-ngx").unwrap();
    let p = Provided::example("paperless-ngx");
    let first = render::render(pl, "rynas", &p, &pl.test_request()).unwrap();
    let compose_text = String::from_utf8(first.file("compose.yaml").unwrap().contents.clone()).unwrap();
    let env_text = String::from_utf8(first.file(".env").unwrap().contents.clone()).unwrap();
    let again = plan::prepare_update(pl, "rynas", &p, "paperless-ngx", &compose_text, &env_text, &BTreeMap::new()).unwrap();
    for s in ["DB_PASSWORD", "SECRET_KEY", "PAPERLESS_ADMIN_PASSWORD", "CONSUME", "PAPERLESS_OCR_LANGUAGE"] {
        assert_eq!(first.env[s], again.env[s], "{s}");
    }
}

/// A broken entry reports every problem with its file.
#[test]
fn lint_reports_broken_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("broken");
    copy_dir(&apps_dir().join("uptime-kuma"), &dst);
    fs::write(
        dst.join("compose.yaml"),
        r#"services:
  uptime-kuma:
    image: louislam/uptime-kuma:latest
    container_name: kuma
    privileged: true
    ports:
      - "3001:3001"
    volumes:
      - ./data:/app/data
      - /var/run/docker.sock:/var/run/docker.sock
    environment:
      X: "${UNDEFINED_VAR:-x}"
"#,
    )
    .unwrap();
    fs::write(dst.join("stray.txt"), "x").unwrap();
    let issues = CatalogApp::load(&dst).unwrap_err();
    let text = issues.to_string();
    for needle in [
        "id `uptime-kuma` differs from the folder name `broken`",
        "stray.txt: unexpected file",
        "moving tag",
        "not pinned by digest",
        "`container_name` is not allowed",
        "privileged is not declared",
        "must be exactly ${ENV}",
        "volume source `./data`",
        "Docker socket mount is not declared",
        "only ${VAR} is allowed",
        "UNDEFINED_VAR} is not provided",
        "is published 0 times",
        "`DATA_FOLDER` is declared but compose never uses",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            fs::copy(e.path(), to).unwrap();
        }
    }
}
