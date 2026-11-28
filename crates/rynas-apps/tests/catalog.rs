//! CI lint of the curated catalogue (`apps/` at the repository root): every entry loads and
//! validates (schema, form, compose structure for every overlay variant, metadata), renders
//! with its `tests/values.toml`, and the rendered stack is an ordinary compose project.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rynas_apps::compose::{self, ImageRef};
use rynas_apps::plan::{self, ImageChangeKind, RamGate, UsedPort};
use rynas_apps::render::{self, Provided};
use rynas_apps::{Catalog, CatalogApp, RenderError, i18n};
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
    // the catalogue moves to a later version (99.0.0, whatever the package's version is now:
    // Renovate bumps it) with a renamed value
    let toml_text = fs::read_to_string(dst.join("app.toml")).unwrap();
    let current = toml_text.lines().find_map(|l| l.strip_prefix("version = \"")).and_then(|v| v.strip_suffix('"')).expect("version line").to_string();
    let toml_text = toml_text.replace(&format!("version = \"{current}\""), "version = \"99.0.0\"");
    fs::write(dst.join("app.toml"), toml_text).unwrap();
    fs::create_dir_all(dst.join("migrations")).unwrap();
    fs::write(
        dst.join("migrations/99.0.0.toml"),
        "to = \"99.0.0\"\ndescription = \"test\"\n[[ops]]\nop = \"set_default\"\nkey = \"EXTRA\"\nvalue = \"x\"\n",
    )
    .unwrap();
    let new = CatalogApp::load(&dst).unwrap_or_else(|e| panic!("{e}"));

    // installed: the current version with an older digest
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
    assert_eq!(plan.from_version.to_string(), current);
    assert_eq!(plan.to_version.to_string(), "99.0.0");
    assert!(!plan.up_to_date && !plan.downgrade);
    assert_eq!(plan.migrations.len(), 1);
    assert_eq!(plan.image_changes.len(), 1);
    assert_eq!(plan.image_changes[0].kind, ImageChangeKind::Digest);

    // the re-render keeps the moved port; the migrated extra value is unknown to the form and
    // stays out of the request
    let updated = plan::prepare_update(&new, "rynas", &Provided::example("uptime-kuma"), "uptime-kuma", &compose_text, &env_text, &BTreeMap::new()).unwrap();
    assert_eq!(updated.env["WEB_PORT"], "3002");
    assert_eq!(updated.meta.version.to_string(), "99.0.0");

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
    privileged: "true"
    ports:
      - "3001:3001"
    volumes:
      - ./data:/app/data
      - /var/run/docker.sock:/var/run/docker.sock
    environment:
      X: "${UNDEFINED_VAR:-x}"
volumes:
  host:
    driver_opts: {type: none, o: bind, device: /}
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
        "named volume `host` must have no options",
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

#[test]
fn translations_are_keyed_by_field_and_checked_against_the_entry() {
    // the shipped ones load (the catalogue validates) and fill the API texts
    let c = catalog();
    let fr = c.get("frigate").unwrap();
    let t = fr.texts("zh-CN").expect("frigate is translated into zh-CN");
    assert_eq!(t.lang, "zh-CN");
    assert!(t.summary.is_some() && t.description.is_some());
    assert_eq!(t.overlays.keys().collect::<Vec<_>>(), vec!["overlays/hw-accel.yaml"]);
    assert_eq!(t.form["LIBVA_DRIVER"].enum_labels.len(), 2);
    assert_eq!(t.notes.len(), fr.manifest.notes.len());
    let card = fr.card_texts("zh-CN").unwrap();
    assert!(card.summary.is_some() && card.form.is_empty() && card.description.is_none());
    assert!(fr.texts("en").is_none() && fr.texts("zh-TW").is_none());

    // a translation that drifted from its entry is refused, with every problem named
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("paperless-ngx");
    copy_dir(&apps_dir().join("paperless-ngx"), &dst);
    fs::write(
        dst.join("i18n/zh-CN.toml"),
        r#"description = "文档"
[ports]
http.purpose = "网页"
[folders]
DATA_FOLDER.description = " "
[shares]
CONSUME.label = "导入"
[secrets]
NOPE.description = "x"
[form]
PAPERLESS_ADMIN_USER.placeholder = "admin"
GONE.title = "x"
[[notes]]
text = "x"
[[groups]]
title = "管理员"
"#,
    )
    .unwrap();
    fs::write(dst.join("i18n/zh-cn.md"), "x").unwrap();
    fs::write(dst.join("i18n/notes.txt"), "x").unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    for needle in [
        "i18n/zh-CN.toml: `ports.http`: no such item in the entry",
        "`folders.DATA_FOLDER.description` is empty",
        "`secrets.NOPE`: no such item",
        "`form.PAPERLESS_ADMIN_USER.placeholder`: the entry has no such text",
        "`form.GONE`: no such item",
        "1 [[notes]] for the 0 notes of app.toml",
        "1 [[groups]] for the 2 x-groups of form.json",
        "i18n/zh-cn.md: `zh-cn` is not a language code",
        "i18n/notes.txt: translations are <lang>.toml or <lang>.md",
        "`description` has no stamp in [sources]",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
    // unknown keys are refused by the parser
    fs::remove_file(dst.join("i18n/zh-cn.md")).unwrap();
    fs::remove_file(dst.join("i18n/notes.txt")).unwrap();
    fs::write(dst.join("i18n/zh-CN.toml"), "summary = \"x\"\n").unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    assert!(text.contains("i18n/zh-CN.toml") && text.contains("summary"), "{text}");
    // a card line too long for the card
    fs::write(dst.join("i18n/zh-CN.toml"), format!("description = \"{}\"\n", "文".repeat(141))).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    assert!(text.contains("longer than 140 characters"), "{text}");
    // the long description alone is a translation too, once stamped
    fs::remove_file(dst.join("i18n/zh-CN.toml")).unwrap();
    assert!(CatalogApp::load(&dst).unwrap_err().to_string().contains("`description.md` has no stamp"));
    assert_eq!(i18n::restamp(&dst).unwrap(), vec![dst.join("i18n/zh-CN.toml")]);
    let app = CatalogApp::load(&dst).unwrap();
    let t = app.texts("zh-CN").unwrap();
    assert!(t.description.is_some() && t.summary.is_none() && app.card_texts("zh-CN").is_none());
}

#[test]
fn translations_follow_the_english_they_were_made_from() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("frigate");
    copy_dir(&apps_dir().join("frigate"), &dst);
    let edit = |file: &str, from: &str, to: &str| {
        let p = dst.join(file);
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains(from), "{from} not in {file}");
        fs::write(p, text.replacen(from, to, 1)).unwrap();
    };
    // a reworded port purpose and swapped notes: the translations are stale
    edit("app.toml", "purpose = \"Web interface", "purpose = \"Web UI");
    let notes: Vec<String> = fs::read_to_string(dst.join("app.toml")).unwrap().lines().filter(|l| l.starts_with("text = ")).map(str::to_owned).collect();
    edit("app.toml", &notes[0], "text = \"@first@\"");
    edit("app.toml", &notes[1], &notes[0]);
    edit("app.toml", "text = \"@first@\"", &notes[1]);
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    for needle in ["`ports.web.purpose`: the English text changed", "`notes[0].text`: the English text changed", "`notes[1].text`: the English text changed"] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
    // after a review: stamped again, the rest of the file untouched, and stable
    let before = fs::read_to_string(dst.join("i18n/zh-CN.toml")).unwrap();
    assert_eq!(i18n::restamp(&dst).unwrap().len(), 1);
    let after = fs::read_to_string(dst.join("i18n/zh-CN.toml")).unwrap();
    assert_eq!(before.split("[sources]").next(), after.split("[sources]").next());
    assert!(CatalogApp::load(&dst).is_ok());
    assert!(i18n::restamp(&dst).unwrap().is_empty(), "a second run changes nothing");
    // a stamp for a text that is not translated
    fs::write(dst.join("i18n/zh-CN.toml"), after.replace("[sources]\n", "[sources]\n\"ports.nope.purpose\" = \"00000000\"\n")).unwrap();
    assert!(CatalogApp::load(&dst).unwrap_err().to_string().contains("[sources] `ports.nope.purpose` stamps no translated text"));
    fs::write(dst.join("i18n/zh-CN.toml"), &after).unwrap();

    // translations need a rynas that reads them
    edit("app.toml", "min_rynas = \"0.9.21\"", "min_rynas = \"0.9.20\"");
    assert!(CatalogApp::load(&dst).unwrap_err().to_string().contains("min_rynas must be at least 0.9.21"));
    edit("app.toml", "min_rynas = \"0.9.20\"", "min_rynas = \"0.9.21\"");
    // `i18n` must be a folder
    fs::remove_dir_all(dst.join("i18n")).unwrap();
    fs::write(dst.join("i18n"), "x").unwrap();
    assert!(CatalogApp::load(&dst).unwrap_err().to_string().contains("frigate/i18n: must be a folder"));
}

#[test]
fn stamps_ignore_line_ends_and_outer_space() {
    assert_eq!(i18n::stamp("a\r\nb\n"), i18n::stamp(" a\nb"));
    assert_ne!(i18n::stamp("Web interface"), i18n::stamp("Web UI"));
    assert_eq!(i18n::stamp("x").len(), 8);
    assert_eq!(i18n::I18N_MIN_RYNAS.to_string(), "0.9.21");
}

fn bind_of<'a>(doc: &'a Value, service: &str, target: &str) -> Option<&'a Value> {
    doc["services"][service]["volumes"].as_sequence()?.iter().find(|v| v.get("target").and_then(Value::as_str) == Some(target))
}

#[test]
fn share_set_renders_one_bind_per_share() {
    let c = catalog();
    let st = c.get("syncthing").unwrap();
    let ss = st.manifest.share_set.as_ref().expect("syncthing has a share set");
    assert_eq!((ss.env.as_str(), ss.target.as_str()), ("SYNC_SHARES", "/shares"));
    let p = Provided::example("syncthing");
    let r = render::render(st, "rynas", &p, &st.test_request()).unwrap();
    assert_eq!(r.env["SYNC_SHARES"], "sync=/volume1/sync;vault=/volume1/vault!");
    let env_text = String::from_utf8(r.file(".env").unwrap().contents.clone()).unwrap();
    assert!(env_text.contains("SYNC_SHARES='sync=/volume1/sync;vault=/volume1/vault!'\n"), "{env_text}");
    let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
    let doc: Value = yaml_serde::from_str(&text).unwrap();
    let sync = bind_of(&doc, "syncthing", "/shares/sync").expect("sync bind");
    assert_eq!(sync["type"].as_str(), Some("bind"));
    assert_eq!(sync["source"].as_str(), Some("/volume1/sync"));
    assert!(sync.get("bind").is_none() && sync.get("read_only").is_none());
    let vault = bind_of(&doc, "syncthing", "/shares/vault").expect("vault bind");
    assert_eq!(vault["bind"]["propagation"].as_str(), Some("rslave"));
    // the config folder stays as it was; the key goes through the environment only
    assert!(text.contains("${CONFIG_FOLDER}:/var/syncthing"));
    assert_eq!(doc["services"]["syncthing"]["environment"]["STGUIAPIKEY"].as_str(), Some("${ST_API_KEY}"));
    assert_eq!(r.env["ST_API_KEY"].len(), 32);
    let shares: Vec<(&str, &str, bool)> = r.folders.iter().filter(|f| f.env.starts_with("SYNC_SHARES/")).map(|f| (f.env.as_str(), f.host_path.as_str(), f.share)).collect();
    assert_eq!(shares, [("SYNC_SHARES/sync", "/volume1/sync", true), ("SYNC_SHARES/vault", "/volume1/vault", true)]);

    // the rendered compose passes the same structural lint as an entry (long binds included)
    for v in doc["services"]["syncthing"]["volumes"].as_sequence().unwrap() {
        if let Value::Mapping(m) = v {
            compose::long_volume_ok(m).unwrap();
        }
    }

    // an empty set: no extra volume; an entry without a set refuses one
    let mut req = st.test_request();
    req.share_set.clear();
    let r = render::render(st, "rynas", &p, &req).unwrap();
    check_rendered(st, &r);
    assert_eq!(r.env["SYNC_SHARES"], "");
    let doc: Value = yaml_serde::from_str(&String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap()).unwrap();
    assert_eq!(doc["services"]["syncthing"]["volumes"].as_sequence().unwrap().len(), 1);
    let vw = c.get("vaultwarden").unwrap();
    let mut req = vw.test_request();
    req.share_set = st.test_request().share_set;
    assert_eq!(render::render(vw, "rynas", &Provided::example("vaultwarden"), &req), Err(RenderError::UnknownValue("share_set".into())));
    // a bad item is an invalid value of the set
    let mut req = st.test_request();
    req.share_set[1].name = "a/b".into();
    assert!(matches!(render::render(st, "rynas", &p, &req), Err(RenderError::InvalidValue { field, .. }) if field == "SYNC_SHARES"));
    // `$` in a path is kept literal in the compose file
    let mut req = st.test_request();
    req.share_set[0].path = "/volume1/a$b".into();
    let r = render::render(st, "rynas", &p, &req).unwrap();
    check_rendered(st, &r);
    assert!(String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap().contains("/volume1/a$$b"));
}

/// An installed 1.x Syncthing (one share at /data, `SYNC_SHARE`) updates to the share set and
/// keeps its folder, an absolute share or the stack's fallback folder.
#[test]
fn syncthing_1x_migrates_to_the_share_set() {
    let c = catalog();
    let st = c.get("syncthing").unwrap();
    let p = Provided::example("syncthing");
    let mut req = st.test_request();
    req.share_set.clear();
    let now = render::render(st, "rynas", &p, &req).unwrap();
    let compose_text = String::from_utf8(now.file("compose.yaml").unwrap().contents.clone()).unwrap().replace("version: 2.0.0", "version: 1.0.0");
    assert!(compose_text.contains("version: 1.0.0"));
    let env_now = String::from_utf8(now.file(".env").unwrap().contents.clone()).unwrap();
    for (old, want_set, share) in [("/volume1/sync", "sync=/volume1/sync", true), ("./sync", "sync=./sync", false)] {
        let env_text: String = env_now
            .lines()
            .filter(|l| !l.starts_with("ST_API_KEY=") && !l.starts_with("SYNC_SHARES="))
            .map(|l| format!("{l}\n"))
            .chain(std::iter::once(format!("SYNC_SHARE={old}\n")))
            .collect();
        let plan = plan::update_plan(st, &compose_text, &env_text).unwrap();
        assert_eq!(plan.migrations.iter().map(|v| v.to_string()).collect::<Vec<_>>(), ["2.0.0"]);
        assert!(plan.missing_values.is_empty());
        let up = plan::prepare_update(st, "rynas", &p, "syncthing", &compose_text, &env_text, &BTreeMap::new()).unwrap();
        check_rendered(st, &up);
        assert_eq!(up.env["SYNC_SHARES"], want_set);
        assert!(!up.env.contains_key("SYNC_SHARE"));
        assert_eq!(up.env["ST_API_KEY"].len(), 32, "the new secret is generated");
        let f = up.folders.iter().find(|f| f.env == "SYNC_SHARES/sync").unwrap();
        assert_eq!(f.share, share);
        if !share {
            assert_eq!(f.host_path, "/volume1/docker/stacks/syncthing/sync");
        }
        let doc: Value = yaml_serde::from_str(&String::from_utf8(up.file("compose.yaml").unwrap().contents.clone()).unwrap()).unwrap();
        assert_eq!(bind_of(&doc, "syncthing", "/shares/sync").unwrap()["source"].as_str(), Some(old));
    }
}

#[test]
fn lint_checks_share_sets_and_long_binds() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("syncthing");
    copy_dir(&apps_dir().join("syncthing"), &dst);
    let t = fs::read_to_string(dst.join("app.toml")).unwrap();
    let broken = t.replace("service = \"syncthing\"\ntarget = \"/shares\"", "service = \"nope\"\ntarget = \"shares/../x\"").replace("env = \"SYNC_SHARES\"", "env = \"WEB_PORT\"");
    fs::write(dst.join("app.toml"), broken).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    for needle in ["share_set: `WEB_PORT` is declared twice", "target `shares/../x` must be an absolute container folder", "names service `nope`"] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
    fs::write(dst.join("app.toml"), t).unwrap();
    let c = fs::read_to_string(dst.join("compose.yaml")).unwrap();
    let bad = c.replace(
        "      - ${CONFIG_FOLDER}:/var/syncthing\n",
        "      - {type: bind, source: \"${CONFIG_FOLDER}\", target: /var/syncthing, bind: {propagation: rshared}}\n      - {type: tmpfs, target: /tmp}\n",
    );
    fs::write(dst.join("compose.yaml"), bad).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    for needle in ["bind propagation `rshared` is not allowed", "volume type `tmpfs` is not allowed"] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
    let ok = c.replace("      - ${CONFIG_FOLDER}:/var/syncthing\n", "      - {type: bind, source: \"${CONFIG_FOLDER}\", target: /var/syncthing, bind: {propagation: rslave}}\n");
    fs::write(dst.join("compose.yaml"), ok).unwrap();
    CatalogApp::load(&dst).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn user_photos_bind_the_staging_folder_read_only() {
    let c = catalog();
    for (id, service, target) in [("immich", "immich-server", "/mnt/photos"), ("pigallery2", "pigallery2", "/app/data/images")] {
        let app = c.get(id).unwrap();
        let up = app.manifest.user_photos.as_ref().expect("user_photos");
        assert_eq!((up.env.as_str(), up.service.as_str(), up.target.as_str()), ("USER_PHOTOS", service, target));
        let p = Provided::example(id);
        let r = render::render(app, "rynas", &p, &app.test_request()).unwrap();
        check_rendered(app, &r);
        let root = format!("/var/lib/rynas/docker/photos/{id}");
        assert_eq!(r.env["USER_PHOTOS"], root);
        let doc: Value = yaml_serde::from_str(&String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap()).unwrap();
        let b = bind_of(&doc, service, target).unwrap_or_else(|| panic!("{id}: staging bind"));
        assert_eq!(b["source"].as_str(), Some(root.as_str()));
        assert_eq!(b["read_only"].as_bool(), Some(true), "{id}: never writable");
        assert_eq!(b["bind"]["propagation"].as_str(), Some("rslave"), "{id}: users' mounts appear without a restart");
        if let Value::Mapping(m) = b {
            compose::long_volume_ok(m).unwrap();
        }
        // never a home, never a per-user bind: rynasd mounts the folders itself
        let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
        assert!(!text.contains("/home/") && !text.contains("Photos"), "{text}");
        assert!(!r.folders.iter().any(|f| f.env.starts_with("USER_PHOTOS")));
        // read back from .env by an update, the API key included
        let mut req = app.test_request();
        if up.key_env.is_some() {
            req.user_photos_key = Some("AbC123-key_xyz".into());
        }
        let r = render::render(app, "rynas", &p, &req).unwrap();
        let back = render::request_from_env(app, id, &r.env);
        assert_eq!(back.user_photos.as_deref(), Some(root.as_str()));
        assert_eq!(back.user_photos_key, req.user_photos_key);
        // none: no bind
        req.user_photos = None;
        let r = render::render(app, "rynas", &p, &req).unwrap();
        assert_eq!(r.env["USER_PHOTOS"], "");
        let doc: Value = yaml_serde::from_str(&String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap()).unwrap();
        assert!(bind_of(&doc, service, target).is_none());
        // never a relative or odd path
        for bad in ["./photos", "relative", "/a;b", "/x/../etc"] {
            let mut req = app.test_request();
            req.user_photos = Some(bad.into());
            assert!(matches!(render::render(app, "rynas", &p, &req), Err(RenderError::InvalidValue { field, .. }) if field == "USER_PHOTOS"), "{bad:?}");
        }
    }
    assert_eq!(c.get("pigallery2").unwrap().manifest.user_photos.as_ref().unwrap().mountpoints, ["Shared"]);
    // the Immich key: kept unquoted, never anything that could break the line
    let im = c.get("immich").unwrap();
    let r = render::render(im, "rynas", &Provided::example("immich"), &im.test_request()).unwrap();
    assert_eq!(r.env["PHOTOS_API_KEY"], "");
    let text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
    assert!(!text.contains("PHOTOS_API_KEY"), "the key never reaches a container");
    let mut req = im.test_request();
    req.user_photos_key = Some("bad key'".into());
    assert!(matches!(render::render(im, "rynas", &Provided::example("immich"), &req), Err(RenderError::InvalidValue { field, .. }) if field == "PHOTOS_API_KEY"));
    // an entry without user_photos refuses them
    let vw = c.get("vaultwarden").unwrap();
    let mut req = vw.test_request();
    req.user_photos = im.test_request().user_photos;
    assert_eq!(render::render(vw, "rynas", &Provided::example("vaultwarden"), &req), Err(RenderError::UnknownValue("user_photos".into())));
}

/// PiGallery2 1.x showed the shared folder as the whole gallery; 2.0.0 moves it to Shared,
/// next to the users' Photos folders, and says so in the update dialog.
#[test]
fn pigallery2_2_moves_the_shared_folder() {
    let c = catalog();
    let pg = c.get("pigallery2").unwrap();
    let p = Provided::example("pigallery2");
    let mut req = pg.test_request();
    req.user_photos = None;
    let now = render::render(pg, "rynas", &p, &req).unwrap();
    let text = String::from_utf8(now.file("compose.yaml").unwrap().contents.clone()).unwrap();
    assert!(text.contains("${PHOTOS}:/app/data/images/Shared:ro"), "{text}");
    let old_compose = text.replace("version: 2.0.0", "version: 1.0.1");
    let old_env: String = String::from_utf8(now.file(".env").unwrap().contents.clone()).unwrap().lines().filter(|l| !l.starts_with("USER_PHOTOS=")).map(|l| format!("{l}\n")).collect();
    let plan = plan::update_plan(pg, &old_compose, &old_env).unwrap();
    assert_eq!(plan.migrations.iter().map(|v| v.to_string()).collect::<Vec<_>>(), ["2.0.0"]);
    assert!(pg.migrations.iter().any(|m| m.description.contains("indexes the photos again")));
    let up = plan::prepare_update(pg, "rynas", &p, "pigallery2", &old_compose, &old_env, &BTreeMap::new()).unwrap();
    check_rendered(pg, &up);
    assert_eq!(up.env["PHOTOS"], "/volume1/photos");
    assert_eq!(up.env["USER_PHOTOS"], "");
}

#[test]
fn lint_checks_user_photos() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("immich");
    copy_dir(&apps_dir().join("immich"), &dst);
    let t = fs::read_to_string(dst.join("app.toml")).unwrap();
    let broken = t
        .replace("service = \"immich-server\"\ntarget = \"/mnt/photos\"", "service = \"nope\"\ntarget = \"mnt/../photos\"")
        .replace("key_env = \"PHOTOS_API_KEY\"", "key_env = \"DB_PASSWORD\"\nmountpoints = [\"ann\"]");
    assert_ne!(broken, t);
    fs::write(dst.join("app.toml"), broken).unwrap();
    let text = CatalogApp::load(&dst).unwrap_err().to_string();
    for needle in ["user_photos: `DB_PASSWORD` is declared twice", "user_photos: target `mnt/../photos` must be an absolute container folder", "names service `nope`", "mount point `ann` must start with an upper-case letter"] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
}

#[test]
fn run_as_a_chosen_user() {
    let c = catalog();
    let st = c.get("syncthing").unwrap();
    assert_eq!(st.manifest.run_as, Some(rynas_apps::manifest::RunAs::User));
    // the caller puts the user's ids in Provided; the name is kept in .env
    let mut p = Provided::example("syncthing");
    (p.puid, p.pgid) = (1026, 100);
    let mut req = st.test_request();
    req.run_as = Some("ann".into());
    let r = render::render(st, "rynas", &p, &req).unwrap();
    check_rendered(st, &r);
    assert_eq!((r.env["RYNAS_RUN_AS"].as_str(), r.env["PUID"].as_str(), r.env["PGID"].as_str()), ("ann", "1026", "100"));
    // the app's own folders are made for that user
    assert_eq!(r.folders.iter().find(|f| f.env == "CONFIG_FOLDER").unwrap().owner, (1026, 100));
    // updates keep it
    let compose_text = String::from_utf8(r.file("compose.yaml").unwrap().contents.clone()).unwrap();
    let env_text = String::from_utf8(r.file(".env").unwrap().contents.clone()).unwrap();
    let again = plan::prepare_update(st, "rynas", &p, "syncthing", &compose_text, &env_text, &BTreeMap::new()).unwrap();
    assert_eq!(again.env["RYNAS_RUN_AS"], "ann");
    assert_eq!(render::request_from_env(st, "syncthing", &render::parse_env(&env_text)).run_as.as_deref(), Some("ann"));
    // refusals: an entry without run_as, a bad name, root
    let vw = c.get("vaultwarden").unwrap();
    let mut vreq = vw.test_request();
    vreq.run_as = Some("ann".into());
    assert_eq!(render::render(vw, "rynas", &Provided::example("vaultwarden"), &vreq), Err(RenderError::UnknownValue("run_as".into())));
    req.run_as = Some("Bad Name".into());
    assert!(matches!(render::render(st, "rynas", &p, &req), Err(RenderError::InvalidValue { field, .. }) if field == "run_as"));
    req.run_as = Some("ann".into());
    (p.puid, p.pgid) = (0, 0);
    assert!(matches!(render::render(st, "rynas", &p, &req), Err(RenderError::InvalidValue { field, .. }) if field == "run_as"));
    // without a choice: the docker-apps ids of Provided, nothing kept
    let r = render::render(st, "rynas", &Provided::example("syncthing"), &st.test_request()).unwrap();
    assert!(!r.env.contains_key("RYNAS_RUN_AS"));
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
