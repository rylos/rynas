//! Thumbnails for the grid and the info panel.
//!
//! In this order:
//! 1. the user's own cache (`/var/cache/rynas/files/thumbs/<uid>/`, made by rynasd, owned by
//!    the user, 0700), keyed by device, inode, size, modification time and size class: a
//!    changed file gets a new thumbnail, nobody else's cache is ever read;
//! 2. on volumes that come from another NAS, the thumbnails its photo app left in `@eaDir`
//!    (only read, never written; only when they exist);
//! 3. made now with ffmpeg (optional: `Recommends`), for photos and videos, and kept in the
//!    cache. Without ffmpeg there is simply no thumbnail and the interface shows the icon.
//!
//! The worker runs as the user: it can only make a thumbnail of a file it can read.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const FFMPEG: &str = "/usr/bin/ffmpeg";
/// A sleeping disk spins up, a big photo decodes: then it gives up.
const FFMPEG_WAIT: Duration = Duration::from_secs(15);
/// A thumbnail bigger than this is not a thumbnail.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Width of a small (grid) and a big (info panel, quick look of a video) thumbnail.
const SMALL: u32 = 320;
const BIG: u32 = 1280;
/// Asked how a HEIF/AVIF photo is made before ffmpeg is asked for its thumbnail.
const FFPROBE: &str = "/usr/bin/ffprobe";
/// ffprobe's description of a photo: a few streams, or a grid of a few hundred tiles.
const PROBE_BYTES: u64 = 4 * 1024 * 1024;
/// More tiles than any camera makes: beyond, ffmpeg's own choice of picture.
const MAX_TILES: usize = 4096;
/// Bumped when thumbnails are made differently: the cached ones are made again.
const CACHE_VERSION: u64 = 2;

/// What ffmpeg is asked to make thumbnails of.
fn makes_thumbnail(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif" | "avif"
            | "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "wmv" | "mpg" | "mpeg" | "m2ts" | "mts" | "ts" | "3gp" | "flv"
    )
}

fn is_video(ext: &str) -> bool {
    matches!(ext, "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "wmv" | "mpg" | "mpeg" | "m2ts" | "mts" | "ts" | "3gp" | "flv")
}

/// Photos that can be a grid of tiles (a phone's HEIC, an AVIF).
pub(crate) fn is_heif(ext: &str) -> bool {
    matches!(ext, "heic" | "heif" | "avif")
}

/// The JPEG thumbnail of `file`, `None` when there is none to give.
pub fn get(file: &Path, big: bool, cache: Option<&Path>) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(file).ok()?;
    if !meta.is_file() {
        return None;
    }
    let cached = cache.map(|dir| cache_path(dir, &meta, big));
    if let Some(p) = &cached
        && let Ok(data) = std::fs::read(p)
    {
        return Some(data);
    }
    if let Some(p) = eadir(file, big) {
        let mut data = Vec::new();
        crate::confine::open(&p).ok()?.take(MAX_BYTES).read_to_end(&mut data).ok()?;
        return Some(data);
    }
    let name = file.file_name()?.to_string_lossy().to_lowercase();
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    if !makes_thumbnail(ext) {
        return None;
    }
    let data = ffmpeg(file, if big { BIG } else { SMALL }, ext)?;
    if let Some(p) = cached {
        store(&p, &data);
    }
    Some(data)
}

/// `<cache>/<2 hex>/<64 hex>.jpg`
fn cache_path(dir: &Path, meta: &std::fs::Metadata, big: bool) -> PathBuf {
    let mut h = Sha256::new();
    for n in [CACHE_VERSION, meta.dev(), meta.ino(), meta.size(), meta.mtime() as u64, meta.mtime_nsec() as u64, u64::from(big)] {
        h.update(n.to_le_bytes());
    }
    let key: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    dir.join(&key[..2]).join(format!("{key}.jpg"))
}

/// Writes a thumbnail into the cache all at once; a failure only means no cache.
fn store(path: &Path, data: &[u8]) {
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::DirBuilder::new().mode(0o700).recursive(true).create(dir);
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .and_then(|mut f| f.write_all(data));
    if written.is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

use std::os::unix::fs::DirBuilderExt;

/// Thumbnails another NAS's photo app left beside the photos (`@eaDir/<name>/…`), largest
/// first up to the size asked.
fn eadir(file: &Path, big: bool) -> Option<PathBuf> {
    let dir = file.parent()?.join("@eaDir").join(file.file_name()?);
    let names: &[&str] = if big {
        &["SYNOPHOTO_THUMB_XL.jpg", "SYNOPHOTO_THUMB_B.jpg", "SYNOPHOTO_THUMB_M.jpg"]
    } else {
        &["SYNOPHOTO_THUMB_M.jpg", "SYNOPHOTO_THUMB_SM.jpg", "SYNOPHOTO_THUMB_B.jpg", "SYNOPHOTO_THUMB_S.jpg"]
    };
    names.iter().map(|n| dir.join(n)).find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
}

/// Whether a thumbnail of a file of this name can be made here (ffmpeg is installed and it
/// is a photo or a video): the list marks those, so the interface asks for them.
pub fn can_make(name: &str) -> bool {
    static FFMPEG_THERE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let lower = name.to_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    makes_thumbnail(ext) && *FFMPEG_THERE.get_or_init(|| Path::new(FFMPEG).exists())
}

/// The files of a folder that have a thumbnail in `@eaDir` already.
pub fn has_eadir(dir: &Path) -> std::collections::HashSet<std::ffi::OsString> {
    crate::confine::read_dir(&dir.join("@eaDir")).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default()
}

/// One JPEG frame scaled to `width` (never enlarged).
fn ffmpeg(file: &Path, width: u32, ext: &str) -> Option<Vec<u8>> {
    if !Path::new(FFMPEG).exists() {
        return None;
    }
    if is_video(ext) {
        // past a black first frame; a short video has no frame at 2 s: its first one
        return frame(file, width, &Plan::Plain, true).or_else(|| frame(file, width, &Plan::Plain, false));
    }
    let plan = if is_heif(ext) { probe(file).map_or(Plan::Plain, |p| plan(&p, width)) } else { Plan::Plain };
    // a grid ffmpeg could not put together: at least what it picks by itself
    frame(file, width, &plan, false).or_else(|| if plan == Plan::Plain { None } else { frame(file, width, &Plan::Plain, false) })
}

/// The JPEG on ffmpeg's stdout, made as `plan` says.
fn frame(file: &Path, width: u32, plan: &Plan, skip: bool) -> Option<Vec<u8>> {
    let (ok, data) = run(FFMPEG, file, MAX_BYTES, |cmd, input| {
        // one thread: a grid asks for many thumbnails at once on a 4-core NAS
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-threads", "1"]);
        // the file alone: one that is really a playlist opens nothing on the network
        cmd.args(["-protocol_whitelist", "file,crypto,data"]);
        if skip {
            cmd.args(["-ss", "2"]);
        }
        if matches!(plan, Plan::Filter(_)) {
            // the turn of a grid is in the filter, the tiles must not turn one by one
            cmd.arg("-noautorotate");
        }
        cmd.arg("-i").arg(input);
        match plan {
            Plan::Plain => {
                cmd.arg("-vf").arg(scale(width));
            }
            Plan::Stream(i) => {
                cmd.arg("-map").arg(format!("0:{i}")).arg("-vf").arg(scale(width));
            }
            Plan::Filter(f) => {
                cmd.arg("-filter_complex").arg(f).args(["-map", "[v]"]);
            }
        }
        cmd.args(["-frames:v", "1", "-q:v", "5", "-f", "image2", "-c:v", "mjpeg", "pipe:1"]);
    })?;
    (ok && !data.is_empty() && data.len() as u64 <= MAX_BYTES && data.starts_with(&[0xff, 0xd8])).then_some(data)
}

fn scale(width: u32) -> String {
    format!("scale='min({width},iw)':-2")
}

/// Runs ffmpeg or ffprobe on `file`, given to it as `file:/proc/self/fd/3`, with nothing of
/// rynasd's environment and a time limit: whether it ended well, and its stdout (at most
/// `cap` + 1 bytes, so a longer one shows).
fn run(prog: &str, file: &Path, cap: u64, args: impl FnOnce(&mut Command, std::ffi::OsString)) -> Option<(bool, Vec<u8>)> {
    // it reads the file opened here (checked to be inside the share), not a path
    let opened = crate::confine::open(file).ok()?;
    let mut cmd = Command::new(prog);
    let mut input = std::ffi::OsString::from("file:");
    input.push(crate::confine::child_input(&mut cmd, &opened).as_os_str());
    args(&mut cmd, input);
    cmd.env_clear().env("PATH", "/usr/bin:/bin").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.by_ref().take(cap + 1).read_to_end(&mut s);
        s
    });
    let start = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if start.elapsed() < FFMPEG_WAIT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    Some((ok, reader.join().ok()?))
}

/// ffprobe's streams and stream groups of a photo. An ffprobe older than 7.1 knows no stream
/// groups, fails, and the photo is made as before.
fn probe(file: &Path) -> Option<serde_json::Value> {
    if !Path::new(FFPROBE).exists() {
        return None;
    }
    let (ok, out) = run(FFPROBE, file, PROBE_BYTES, |cmd, input| {
        cmd.args(["-hide_banner", "-loglevel", "error", "-protocol_whitelist", "file,crypto,data"])
            .args(["-of", "json", "-show_streams", "-show_stream_groups"])
            .arg(input);
    })?;
    if !ok || out.len() as u64 > PROBE_BYTES {
        return None;
    }
    serde_json::from_slice(&out).ok()
}

/// The size in pixels of a HEIF/AVIF photo made of tiles, as it is shown (turned).
pub(crate) fn heif_size(file: &Path) -> Option<(u64, u64)> {
    grid(&probe(file)?).map(|g| g.shown())
}

/// How ffmpeg is asked for the picture.
#[derive(Debug, PartialEq)]
enum Plan {
    /// ffmpeg's own choice of stream: every file but a HEIF grid
    Plain,
    /// this stream: the photo's own preview, big enough and of the same shape
    Stream(u64),
    /// the tiles put together by this `-filter_complex`, whose output is `[v]`
    Filter(String),
}

/// A picture made of tiles (HEIF `grid`), as ffprobe 7.1 and later describe it.
#[derive(Debug, PartialEq)]
struct Grid {
    width: u64,
    height: u64,
    x: u64,
    y: u64,
    /// stream index, left and top of each tile
    tiles: Vec<(u64, u64, u64)>,
    /// degrees clockwise ffmpeg would turn it (0, 90, 180, 270)
    turn: u32,
}

impl Grid {
    fn shown(&self) -> (u64, u64) {
        if self.turn % 180 == 90 { (self.height, self.width) } else { (self.width, self.height) }
    }
}

fn plan(probe: &serde_json::Value, width: u32) -> Plan {
    let Some(g) = grid(probe) else { return Plan::Plain };
    match preview_stream(probe, &g, width) {
        Some(i) => Plan::Stream(i),
        None => Plan::Filter(grid_filter(&g, width)),
    }
}

fn stream(probe: &serde_json::Value, index: u64) -> Option<&serde_json::Value> {
    probe.get("streams")?.as_array()?.iter().find(|s| s.get("index").and_then(serde_json::Value::as_u64) == Some(index))
}

/// The photo's grid: the default one when there are more (a gain map is a grid too).
fn grid(probe: &serde_json::Value) -> Option<Grid> {
    use serde_json::Value;
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    let groups: Vec<&Value> = probe
        .get("stream_groups")?
        .as_array()?
        .iter()
        .filter(|g| g.get("type").and_then(Value::as_str) == Some("Tile Grid"))
        .collect();
    let group = groups
        .iter()
        .find(|g| g.get("disposition").and_then(|d| num(d, "default")) == Some(1))
        .or_else(|| groups.first())?;
    let c = group.get("components")?.as_array()?.first()?;
    let tiles: Vec<(u64, u64, u64)> = c
        .get("subcomponents")?
        .as_array()?
        .iter()
        .map(|t| Some((num(t, "stream_index")?, num(t, "tile_horizontal_offset")?, num(t, "tile_vertical_offset")?)))
        .collect::<Option<_>>()?;
    let (width, height) = (num(c, "width")?, num(c, "height")?);
    if tiles.is_empty() || tiles.len() > MAX_TILES || width == 0 || height == 0 {
        return None;
    }
    // the turn is on the grid; a tile that carries one stands for the grid
    let turn = [Some(c), Some(*group), stream(probe, tiles[0].0)].into_iter().flatten().find_map(turn).unwrap_or(0);
    Some(Grid { width, height, x: num(c, "horizontal_offset").unwrap_or(0), y: num(c, "vertical_offset").unwrap_or(0), tiles, turn })
}

/// The clockwise turn ffmpeg's autorotate makes for a display matrix: ffprobe gives its
/// `rotation` counterclockwise.
fn turn(v: &serde_json::Value) -> Option<u32> {
    let r = v
        .get("side_data_list")?
        .as_array()?
        .iter()
        .filter(|s| s.get("side_data_type").and_then(serde_json::Value::as_str) == Some("Display Matrix"))
        .find_map(|s| s.get("rotation")?.as_f64())?;
    Some(((-r / 90.0).round() as i64).rem_euclid(4) as u32 * 90)
}

/// A stream that is a whole picture of its own (not a tile, not an auxiliary image), at least
/// about as wide as asked, with the grid's shape: the photo's preview, read in a moment.
fn preview_stream(probe: &serde_json::Value, g: &Grid, width: u32) -> Option<u64> {
    use serde_json::Value;
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    probe
        .get("streams")?
        .as_array()?
        .iter()
        .filter(|s| {
            let (w, h) = (num(s, "width"), num(s, "height"));
            let shown = if g.turn % 180 == 90 { h } else { w };
            s.get("codec_type").and_then(Value::as_str) == Some("video")
                && s.get("disposition").is_some_and(|d| num(d, "dependent") == 0 && num(d, "attached_pic") == 0)
                && !g.tiles.iter().any(|t| t.0 == num(s, "index"))
                && w > 0
                && h > 0
                && shown * 5 >= u64::from(width) * 4
                // the same shape, within 2 %
                && (w * g.height).abs_diff(h * g.width) * 50 <= w * g.height
        })
        .max_by_key(|s| num(s, "width"))
        .map(|s| num(s, "index"))
}

/// `[0:0][0:1]…xstack=…,crop=…[,turn],scale=…[v]`: the tiles where the grid puts them, cut to
/// the picture, turned as ffmpeg would turn it, scaled.
fn grid_filter(g: &Grid, width: u32) -> String {
    let mut f: String = g.tiles.iter().map(|(i, _, _)| format!("[0:{i}]")).collect();
    if g.tiles.len() > 1 {
        let layout: Vec<String> = g.tiles.iter().map(|(_, x, y)| format!("{x}_{y}")).collect();
        f += &format!("xstack=inputs={}:layout={}:fill=black,", g.tiles.len(), layout.join("|"));
    }
    f += &format!("crop={}:{}:{}:{}", g.width, g.height, g.x, g.y);
    f += match g.turn {
        90 => ",transpose=clock",
        180 => ",hflip,vflip",
        270 => ",transpose=cclock",
        _ => "",
    };
    f + "," + &scale(width) + "[v]"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_follow_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"one").unwrap();
        let m1 = std::fs::metadata(&f).unwrap();
        let small = cache_path(Path::new("/c"), &m1, false);
        assert_ne!(small, cache_path(Path::new("/c"), &m1, true));
        std::fs::write(&f, b"three").unwrap();
        let m2 = std::fs::metadata(&f).unwrap();
        assert_ne!(small, cache_path(Path::new("/c"), &m2, false));
        assert!(small.starts_with("/c") && small.extension().unwrap() == "jpg");
    }

    #[test]
    fn served_from_the_cache_and_from_eadir() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let f = dir.path().join("p.jpg");
        std::fs::write(&f, b"not really a photo").unwrap();
        // nothing yet (and no thumbnail of a non-media file ever)
        let txt = dir.path().join("t.txt");
        std::fs::write(&txt, b"x").unwrap();
        assert!(get(&txt, false, Some(&cache)).is_none());
        // @eaDir of another NAS
        let ea = dir.path().join("@eaDir/p.jpg");
        std::fs::create_dir_all(&ea).unwrap();
        std::fs::write(ea.join("SYNOPHOTO_THUMB_M.jpg"), b"\xff\xd8thumb").unwrap();
        assert_eq!(get(&f, false, Some(&cache)).unwrap(), b"\xff\xd8thumb");
        // the cache wins
        let p = cache_path(&cache, &std::fs::metadata(&f).unwrap(), false);
        store(&p, b"\xff\xd8cached");
        assert_eq!(get(&f, false, Some(&cache)).unwrap(), b"\xff\xd8cached");
        assert!(has_eadir(dir.path()).contains(std::ffi::OsStr::new("p.jpg")));
    }

    /// A phone's HEIC (4000×2252 in 8×5 tiles of 512, with a 512×288 preview), as ffprobe
    /// 7.1 describes it, trimmed.
    fn phone() -> serde_json::Value {
        serde_json::from_str(include_str!("testdata/heic-grid-probe.json")).unwrap()
    }

    #[test]
    fn a_phone_photo_is_a_grid_of_tiles() {
        let g = grid(&phone()).unwrap();
        assert_eq!((g.width, g.height, g.x, g.y, g.turn), (4000, 2252, 0, 0, 0));
        assert_eq!(g.tiles.len(), 40);
        assert_eq!(g.tiles[1], (1, 512, 0));
        assert_eq!(g.tiles[39], (39, 3584, 2048));
        assert_eq!(g.shown(), (4000, 2252));
    }

    #[test]
    fn small_thumbnails_come_from_the_preview_big_ones_from_the_tiles() {
        let p = phone();
        // 512 wide, the photo's shape: enough for the grid
        assert_eq!(plan(&p, SMALL), Plan::Stream(40));
        let Plan::Filter(f) = plan(&p, BIG) else { panic!("{:?}", plan(&p, BIG)) };
        assert!(f.starts_with("[0:0][0:1][0:2]"), "{f}");
        assert!(f.contains("[0:39]xstack=inputs=40:layout=0_0|512_0|1024_0|"), "{f}");
        assert!(f.ends_with("|3584_2048:fill=black,crop=4000:2252:0:0,scale='min(1280,iw)':-2[v]"), "{f}");
        assert!(!f.contains("[0:40]") && !f.contains("transpose"), "{f}");
    }

    #[test]
    fn without_a_grid_ffmpeg_chooses() {
        // an ffprobe older than 7.1, an ordinary HEIC, nothing at all
        let plain = serde_json::json!({"streams": [{"index": 0, "codec_type": "video", "width": 4032, "height": 3024, "disposition": {"dependent": 0}}]});
        assert_eq!(plan(&plain, BIG), Plan::Plain);
        assert_eq!(plan(&serde_json::json!({}), SMALL), Plan::Plain);
        let other = serde_json::json!({"stream_groups": [{"type": "IAMF Audio Element", "components": []}], "streams": []});
        assert_eq!(plan(&other, SMALL), Plan::Plain);
    }

    #[test]
    fn a_turned_grid_is_turned_after_the_cut() {
        // two tiles side by side, a portrait photo taken turned; the preview has another shape
        let p = serde_json::json!({
            "stream_groups": [
                {"type": "Tile Grid", "disposition": {"default": 0}, "components": [{"width": 10, "height": 10, "subcomponents": [
                    {"stream_index": 5, "tile_horizontal_offset": 0, "tile_vertical_offset": 0}]}]},
                {"type": "Tile Grid", "disposition": {"default": 1}, "components": [{
                    "width": 1000, "height": 500, "horizontal_offset": 8, "vertical_offset": 4,
                    "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}],
                    "subcomponents": [
                        {"stream_index": 0, "tile_horizontal_offset": 0, "tile_vertical_offset": 0},
                        {"stream_index": 1, "tile_horizontal_offset": 512, "tile_vertical_offset": 0}]}]}],
            "streams": [
                {"index": 0, "codec_type": "video", "width": 512, "height": 512, "disposition": {"dependent": 1}},
                {"index": 1, "codec_type": "video", "width": 512, "height": 512, "disposition": {"dependent": 1}},
                {"index": 2, "codec_type": "video", "width": 512, "height": 512, "disposition": {"dependent": 0}}]
        });
        let g = grid(&p).unwrap();
        assert_eq!((g.turn, g.shown()), (90, (500, 1000)));
        assert_eq!(
            plan(&p, SMALL),
            Plan::Filter("[0:0][0:1]xstack=inputs=2:layout=0_0|512_0:fill=black,crop=1000:500:8:4,transpose=clock,scale='min(320,iw)':-2[v]".into())
        );
        // the other way round, upside down
        let mut q = p.clone();
        q["stream_groups"][1]["components"][0]["side_data_list"][0]["rotation"] = serde_json::json!(90);
        assert_eq!(grid(&q).unwrap().turn, 270);
        q["stream_groups"][1]["components"][0]["side_data_list"][0]["rotation"] = serde_json::json!(180.0);
        assert!(matches!(plan(&q, SMALL), Plan::Filter(f) if f.contains(",hflip,vflip,")));
    }

    #[test]
    fn a_single_tile_is_only_cut() {
        let p = serde_json::json!({
            "stream_groups": [{"type": "Tile Grid", "components": [{"width": 500, "height": 300, "subcomponents": [
                {"stream_index": 3, "tile_horizontal_offset": 0, "tile_vertical_offset": 0}]}]}],
            "streams": [{"index": 3, "codec_type": "video", "width": 512, "height": 320, "disposition": {"dependent": 1}}]
        });
        assert_eq!(plan(&p, BIG), Plan::Filter("[0:3]crop=500:300:0:0,scale='min(1280,iw)':-2[v]".into()));
    }

    #[test]
    fn a_preview_too_small_is_not_used() {
        let mut p = phone();
        p["streams"][40]["width"] = serde_json::json!(160);
        p["streams"][40]["height"] = serde_json::json!(90);
        assert!(matches!(plan(&p, SMALL), Plan::Filter(_)));
        // 0.8 of the width asked is enough
        p["streams"][40]["width"] = serde_json::json!(256);
        p["streams"][40]["height"] = serde_json::json!(144);
        assert_eq!(plan(&p, SMALL), Plan::Stream(40));
    }
}
