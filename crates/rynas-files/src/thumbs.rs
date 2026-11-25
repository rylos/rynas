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
    let plain = Plan::Plain(Vec::new());
    if is_video(ext) {
        // past a black first frame; a short video has no frame at 2 s: its first one
        return frame(file, width, &plain, true).or_else(|| frame(file, width, &plain, false));
    }
    let plan = if is_heif(ext) { probe(file).map_or(Plan::Plain(Vec::new()), |p| plan(&p, width, &orientation(file))) } else { Plan::Plain(Vec::new()) };
    // a grid ffmpeg could not put together: at least what it picks by itself
    frame(file, width, &plan, false).or_else(|| if plan == plain { None } else { frame(file, width, &plain, false) })
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
        if !matches!(plan, Plan::Plain(ops) if ops.is_empty()) {
            // the turn is in the filter: ffmpeg must not turn the picture (or each tile) too
            cmd.arg("-noautorotate");
        }
        cmd.arg("-i").arg(input);
        match plan {
            Plan::Plain(ops) => {
                cmd.arg("-vf").arg(chain(ops, width));
            }
            Plan::Stream(i, ops) => {
                cmd.arg("-map").arg(format!("0:{i}")).arg("-vf").arg(chain(ops, width));
            }
            Plan::Filter(f) => {
                cmd.arg("-filter_complex").arg(f).args(["-map", "[v]"]);
            }
        }
        cmd.args(["-frames:v", "1", "-q:v", "5", "-f", "image2", "-c:v", "mjpeg", "pipe:1"]);
    })?;
    (ok && !data.is_empty() && data.len() as u64 <= MAX_BYTES && data.starts_with(&[0xff, 0xd8])).then_some(data)
}

/// The turns and flips, then the scale.
fn chain(ops: &[Op], width: u32) -> String {
    ops.iter().map(|o| o.filter().to_string()).chain([scale(width)]).collect::<Vec<_>>().join(",")
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
    let g = grid(&probe(file)?)?;
    let ops = g.ops.clone().unwrap_or_else(|| orientation(file));
    Some(g.shown(&ops))
}

/// A turn or a flip, as ffmpeg's filter makes it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Clock,
    Cclock,
    Half,
    HFlip,
    VFlip,
    /// across the diagonal from top left (EXIF 5)
    Transpose,
    /// across the other diagonal (EXIF 7)
    Transverse,
}

impl Op {
    fn filter(self) -> &'static str {
        match self {
            Op::Clock => "transpose=clock",
            Op::Cclock => "transpose=cclock",
            Op::Half => "hflip,vflip",
            Op::HFlip => "hflip",
            Op::VFlip => "vflip",
            Op::Transpose => "transpose=cclock_flip",
            Op::Transverse => "transpose=clock_flip",
        }
    }

    fn swaps(self) -> bool {
        matches!(self, Op::Clock | Op::Cclock | Op::Transpose | Op::Transverse)
    }
}

fn swaps(ops: &[Op]) -> bool {
    ops.iter().filter(|o| o.swaps()).count() % 2 == 1
}

/// How ffmpeg is asked for the picture.
#[derive(Debug, PartialEq)]
enum Plan {
    /// ffmpeg's own choice of stream, turned and flipped like this (none: ffmpeg turns it
    /// itself as its display matrix says)
    Plain(Vec<Op>),
    /// this stream, the photo's own preview (big enough, of the same shape), turned like this
    Stream(u64, Vec<Op>),
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
    /// the turn ffprobe gives (a display matrix), when it gives one
    ops: Option<Vec<Op>>,
}

impl Grid {
    fn shown(&self, ops: &[Op]) -> (u64, u64) {
        if swaps(ops) { (self.height, self.width) } else { (self.width, self.height) }
    }
}

/// `file_ops`: the turn the file itself gives (HEIF `irot`/`imir`, or its EXIF), used where
/// ffprobe gives none.
fn plan(probe: &serde_json::Value, width: u32, file_ops: &[Op]) -> Plan {
    let Some(g) = grid(probe) else {
        // a single picture: ffmpeg turns it by itself when it knows the turn
        let known = probe.get("streams").and_then(serde_json::Value::as_array).is_some_and(|s| s.iter().any(|s| turn(s).is_some()));
        return Plan::Plain(if known { Vec::new() } else { file_ops.to_vec() });
    };
    let ops = g.ops.clone().unwrap_or_else(|| file_ops.to_vec());
    match preview_stream(probe, &g, swaps(&ops), width) {
        Some(i) => Plan::Stream(i, stream(probe, i).and_then(turn).unwrap_or(ops)),
        None => Plan::Filter(grid_filter(&g, &ops, width)),
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
    let ops = [Some(c), Some(*group), stream(probe, tiles[0].0)].into_iter().flatten().find_map(turn);
    Some(Grid { width, height, x: num(c, "horizontal_offset").unwrap_or(0), y: num(c, "vertical_offset").unwrap_or(0), tiles, ops })
}

/// The clockwise turn ffmpeg's autorotate makes for a display matrix: ffprobe gives its
/// `rotation` counterclockwise.
fn turn(v: &serde_json::Value) -> Option<Vec<Op>> {
    let r = v
        .get("side_data_list")?
        .as_array()?
        .iter()
        .filter(|s| s.get("side_data_type").and_then(serde_json::Value::as_str) == Some("Display Matrix"))
        .find_map(|s| s.get("rotation")?.as_f64())?;
    Some(match ((-r / 90.0).round() as i64).rem_euclid(4) {
        1 => vec![Op::Clock],
        2 => vec![Op::Half],
        3 => vec![Op::Cclock],
        _ => Vec::new(),
    })
}

/// A stream that is a whole picture of its own (not a tile, not an auxiliary image), at least
/// about as wide as asked once turned (`swapped`), with the grid's shape: the photo's preview,
/// read in a moment.
fn preview_stream(probe: &serde_json::Value, g: &Grid, swapped: bool, width: u32) -> Option<u64> {
    use serde_json::Value;
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    probe
        .get("streams")?
        .as_array()?
        .iter()
        .filter(|s| {
            let (w, h) = (num(s, "width"), num(s, "height"));
            let shown = if swapped { h } else { w };
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

/// The turn the file itself gives: the HEIF transforms of its primary item, else its EXIF
/// Orientation.
fn orientation(file: &Path) -> Vec<Op> {
    let boxes = crate::confine::open(file).ok().and_then(|mut f| heif_transform(&mut f));
    boxes.or_else(|| crate::media::exif_orientation(file).map(exif_ops)).unwrap_or_default()
}

fn exif_ops(orientation: u32) -> Vec<Op> {
    match orientation {
        2 => vec![Op::HFlip],
        3 => vec![Op::Half],
        4 => vec![Op::VFlip],
        5 => vec![Op::Transpose],
        6 => vec![Op::Clock],
        7 => vec![Op::Transverse],
        8 => vec![Op::Cclock],
        _ => Vec::new(),
    }
}

/// The biggest `meta` box read: item properties, not pictures.
const META_BYTES: u64 = 1024 * 1024;

/// The `irot`/`imir` of a HEIF's primary item, in the order they apply; `None` when it has
/// none. Only the top-level box headers and the `meta` box are read (it can be after `mdat`,
/// which is skipped).
fn heif_transform<R: std::io::Read + std::io::Seek>(r: &mut R) -> Option<Vec<Op>> {
    use std::io::SeekFrom;
    let mut pos = 0u64;
    for _ in 0..64 {
        r.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 8];
        r.read_exact(&mut h).ok()?;
        let (mut size, mut header) = (u64::from(u32::from_be_bytes([h[0], h[1], h[2], h[3]])), 8);
        if size == 1 {
            let mut l = [0u8; 8];
            r.read_exact(&mut l).ok()?;
            (size, header) = (u64::from_be_bytes(l), 16);
        } else if size == 0 {
            // up to the end of the file
            size = r.seek(SeekFrom::End(0)).ok()?.checked_sub(pos)?;
        }
        if size < header {
            return None;
        }
        if &h[4..] == b"meta" {
            let len = size - header;
            if len > META_BYTES {
                return None;
            }
            r.seek(SeekFrom::Start(pos + header)).ok()?;
            let mut meta = vec![0; len as usize];
            r.read_exact(&mut meta).ok()?;
            return meta_transform(&meta);
        }
        pos = pos.checked_add(size)?;
    }
    None
}

/// The boxes inside a box's payload: type and payload.
fn children(mut d: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while d.len() >= 8 {
        let kind = [d[4], d[5], d[6], d[7]];
        let (header, size) = match u32::from_be_bytes([d[0], d[1], d[2], d[3]]) {
            0 => (8, d.len() as u64),
            1 if d.len() >= 16 => (16, u64::from_be_bytes(d[8..16].try_into().unwrap_or([0; 8]))),
            1 => break,
            s => (8, u64::from(s)),
        };
        if size < header || size > d.len() as u64 {
            break;
        }
        out.push((kind, &d[header as usize..size as usize]));
        d = &d[size as usize..];
    }
    out
}

/// `n` bytes big-endian off the front of `d`.
fn take(d: &mut &[u8], n: usize) -> Option<u32> {
    let (a, b) = d.split_at_checked(n)?;
    *d = b;
    Some(a.iter().fold(0, |v, &x| (v << 8) | u32::from(x)))
}

/// The payload of `meta` (a full box): `pitm` names the primary item, `iprp/ipma` the
/// properties of each item (1-based indexes into `iprp/ipco`).
fn meta_transform(meta: &[u8]) -> Option<Vec<Op>> {
    let kids = children(meta.get(4..)?);
    fn find<'a>(kids: &[([u8; 4], &'a [u8])], k: &[u8; 4]) -> Option<&'a [u8]> {
        kids.iter().find(|c| &c.0 == k).map(|c| c.1)
    }
    let mut pitm = find(&kids, b"pitm")?;
    let version = take(&mut pitm, 4)? >> 24;
    let primary = take(&mut pitm, if version == 0 { 2 } else { 4 })?;
    let iprp = children(find(&kids, b"iprp")?);
    let props = children(find(&iprp, b"ipco")?);
    let mut ops = Vec::new();
    let mut found = false;
    for ipma in iprp.iter().filter(|c| &c.0 == b"ipma").map(|c| c.1) {
        for index in associations(ipma, primary).unwrap_or_default() {
            let Some((kind, p)) = index.checked_sub(1).and_then(|i| props.get(i)) else { continue };
            match (kind, p.first()) {
                (b"irot", Some(a)) => {
                    found = true;
                    // anticlockwise quarter turns
                    ops.extend(match a & 3 {
                        1 => Some(Op::Cclock),
                        2 => Some(Op::Half),
                        3 => Some(Op::Clock),
                        _ => None,
                    });
                }
                (b"imir", Some(a)) => {
                    found = true;
                    // axis 0 is vertical: left and right change places
                    ops.push(if a & 1 == 0 { Op::HFlip } else { Op::VFlip });
                }
                _ => {}
            }
        }
    }
    found.then_some(ops)
}

/// The property indexes `ipma` gives `item`.
fn associations(mut d: &[u8], item: u32) -> Option<Vec<usize>> {
    let head = take(&mut d, 4)?;
    let (version, wide) = (head >> 24, head & 1 == 1);
    for _ in 0..take(&mut d, 4)? {
        let id = take(&mut d, if version == 0 { 2 } else { 4 })?;
        let mut list = Vec::new();
        for _ in 0..take(&mut d, 1)? {
            // the top bit says "essential"
            list.push(if wide { take(&mut d, 2)? & 0x7fff } else { take(&mut d, 1)? & 0x7f } as usize);
        }
        if id == item {
            return Some(list);
        }
    }
    None
}

/// `[0:0][0:1]…xstack=…,crop=…[,turn],scale=…[v]`: the tiles where the grid puts them, cut to
/// the picture, turned as ffmpeg would turn it, scaled.
fn grid_filter(g: &Grid, ops: &[Op], width: u32) -> String {
    let mut f: String = g.tiles.iter().map(|(i, _, _)| format!("[0:{i}]")).collect();
    if g.tiles.len() > 1 {
        let layout: Vec<String> = g.tiles.iter().map(|(_, x, y)| format!("{x}_{y}")).collect();
        f += &format!("xstack=inputs={}:layout={}:fill=black,", g.tiles.len(), layout.join("|"));
    }
    f += &format!("crop={}:{}:{}:{}", g.width, g.height, g.x, g.y);
    f + "," + &chain(ops, width) + "[v]"
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
        assert_eq!((g.width, g.height, g.x, g.y, &g.ops), (4000, 2252, 0, 0, &None));
        assert_eq!(g.tiles.len(), 40);
        assert_eq!(g.tiles[1], (1, 512, 0));
        assert_eq!(g.tiles[39], (39, 3584, 2048));
        assert_eq!(g.shown(&[]), (4000, 2252));
        // ffmpeg 7.1 says nothing of the turn: the file's own (a portrait photo)
        assert_eq!(g.shown(&[Op::Cclock]), (2252, 4000));
    }

    #[test]
    fn small_thumbnails_come_from_the_preview_big_ones_from_the_tiles() {
        let p = phone();
        // 512 wide, the photo's shape: enough for the grid
        assert_eq!(plan(&p, SMALL, &[]), Plan::Stream(40, vec![]));
        let Plan::Filter(f) = plan(&p, BIG, &[]) else { panic!("{:?}", plan(&p, BIG, &[])) };
        assert!(f.starts_with("[0:0][0:1][0:2]"), "{f}");
        assert!(f.contains("[0:39]xstack=inputs=40:layout=0_0|512_0|1024_0|"), "{f}");
        assert!(f.ends_with("|3584_2048:fill=black,crop=4000:2252:0:0,scale='min(1280,iw)':-2[v]"), "{f}");
        assert!(!f.contains("[0:40]") && !f.contains("transpose"), "{f}");
    }

    #[test]
    fn without_a_grid_ffmpeg_chooses() {
        // an ffprobe older than 7.1, an ordinary HEIC, nothing at all
        let plain = serde_json::json!({"streams": [{"index": 0, "codec_type": "video", "width": 4032, "height": 3024, "disposition": {"dependent": 0}}]});
        assert_eq!(plan(&plain, BIG, &[]), Plan::Plain(vec![]));
        assert_eq!(plan(&serde_json::json!({}), SMALL, &[]), Plan::Plain(vec![]));
        let other = serde_json::json!({"stream_groups": [{"type": "IAMF Audio Element", "components": []}], "streams": []});
        assert_eq!(plan(&other, SMALL, &[]), Plan::Plain(vec![]));
        // the file's turn when ffmpeg knows none, ffmpeg's own when it knows one
        assert_eq!(plan(&plain, BIG, &[Op::Clock]), Plan::Plain(vec![Op::Clock]));
        let mut turned = plain.clone();
        turned["streams"][0]["side_data_list"] = serde_json::json!([{"side_data_type": "Display Matrix", "rotation": -90}]);
        assert_eq!(plan(&turned, BIG, &[Op::Clock]), Plan::Plain(vec![]));
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
        assert_eq!(g.ops, Some(vec![Op::Clock]));
        assert_eq!(g.shown(&[Op::Clock]), (500, 1000));
        assert_eq!(
            // ffprobe's turn wins over the file's
            plan(&p, SMALL, &[Op::HFlip]),
            Plan::Filter("[0:0][0:1]xstack=inputs=2:layout=0_0|512_0:fill=black,crop=1000:500:8:4,transpose=clock,scale='min(320,iw)':-2[v]".into())
        );
        // the other way round, upside down
        let mut q = p.clone();
        q["stream_groups"][1]["components"][0]["side_data_list"][0]["rotation"] = serde_json::json!(90);
        assert_eq!(grid(&q).unwrap().ops, Some(vec![Op::Cclock]));
        q["stream_groups"][1]["components"][0]["side_data_list"][0]["rotation"] = serde_json::json!(180.0);
        assert!(matches!(plan(&q, SMALL, &[]), Plan::Filter(f) if f.contains(",hflip,vflip,")));
    }

    #[test]
    fn a_single_tile_is_only_cut() {
        let p = serde_json::json!({
            "stream_groups": [{"type": "Tile Grid", "components": [{"width": 500, "height": 300, "subcomponents": [
                {"stream_index": 3, "tile_horizontal_offset": 0, "tile_vertical_offset": 0}]}]}],
            "streams": [{"index": 3, "codec_type": "video", "width": 512, "height": 320, "disposition": {"dependent": 1}}]
        });
        assert_eq!(plan(&p, BIG, &[]), Plan::Filter("[0:3]crop=500:300:0:0,scale='min(1280,iw)':-2[v]".into()));
    }

    #[test]
    fn a_preview_too_small_is_not_used() {
        let mut p = phone();
        p["streams"][40]["width"] = serde_json::json!(160);
        p["streams"][40]["height"] = serde_json::json!(90);
        assert!(matches!(plan(&p, SMALL, &[]), Plan::Filter(_)));
        // 0.8 of the width asked is enough
        p["streams"][40]["width"] = serde_json::json!(256);
        p["streams"][40]["height"] = serde_json::json!(144);
        assert_eq!(plan(&p, SMALL, &[]), Plan::Stream(40, vec![]));
        // turned, the preview is as wide as it is high
        assert!(matches!(plan(&p, SMALL, &[Op::Clock]), Plan::Filter(_)));
    }

    #[test]
    fn a_portrait_phone_photo_is_turned_as_its_file_says() {
        // what ffmpeg 7.1 gives for a Samsung portrait photo: no display matrix, irot 1
        let p = phone();
        let ops = [Op::Cclock];
        assert_eq!(plan(&p, SMALL, &ops), Plan::Stream(40, vec![Op::Cclock]));
        assert_eq!(chain(&ops, SMALL), "transpose=cclock,scale='min(320,iw)':-2");
        let Plan::Filter(f) = plan(&p, BIG, &ops) else { panic!() };
        assert!(f.ends_with(":fill=black,crop=4000:2252:0:0,transpose=cclock,scale='min(1280,iw)':-2[v]"), "{f}");
        assert_eq!(exif_ops(6), vec![Op::Clock]);
        assert_eq!(exif_ops(1), vec![]);
    }

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(payload);
        b
    }

    /// ftyp, a big mdat, then meta: primary item 1 (a grid), properties ispe, irot, imir;
    /// `ipma` gives item `owner` the properties `assoc` (1-based, top bit essential).
    fn heif_file(angle: u8, owner: u16, assoc: &[u8], wide: bool) -> Vec<u8> {
        let ispe = bx(b"ispe", &[0; 12]);
        let ipco = bx(b"ipco", &[ispe, bx(b"irot", &[angle]), bx(b"imir", &[1])].concat());
        let mut ipma = vec![0, 0, 0, u8::from(wide), 0, 0, 0, 2];
        for (id, list) in [(2u16, &[1u8][..]), (owner, assoc)] {
            ipma.extend_from_slice(&id.to_be_bytes());
            ipma.push(list.len() as u8);
            for &a in list {
                if wide {
                    ipma.extend_from_slice(&u16::from(a & 0x7f).to_be_bytes());
                } else {
                    ipma.push(a);
                }
            }
        }
        let iprp = bx(b"iprp", &[ipco, bx(b"ipma", &ipma)].concat());
        let meta = bx(b"meta", &[vec![0; 4], bx(b"hdlr", &[0; 24]), bx(b"pitm", &[0, 0, 0, 0, 0, 1]), iprp].concat());
        [bx(b"ftyp", b"heic\0\0\0\0mif1heic"), bx(b"mdat", &vec![0xaa; 100_000]), meta].concat()
    }

    #[test]
    fn the_turn_of_the_primary_item_is_read_from_the_meta_box() {
        let read = |f: Vec<u8>| heif_transform(&mut std::io::Cursor::new(f));
        assert_eq!(read(heif_file(1, 1, &[0x81, 2], false)), Some(vec![Op::Cclock]));
        assert_eq!(read(heif_file(3, 1, &[1, 0x82], false)), Some(vec![Op::Clock]));
        assert_eq!(read(heif_file(3, 1, &[1, 2], true)), Some(vec![Op::Clock]));
        // turned then mirrored, in the order of the associations
        assert_eq!(read(heif_file(1, 1, &[2, 3], false)), Some(vec![Op::Cclock, Op::VFlip]));
        // irot 0 is a turn: nothing, and no EXIF either
        assert_eq!(read(heif_file(0, 1, &[2], false)), Some(vec![]));
        // the irot belongs to another item: the primary has none
        assert_eq!(read(heif_file(1, 3, &[2], false)), None);
        assert_eq!(read(heif_file(1, 1, &[1], false)), None);
        // not a HEIF, a cut file, a meta box too big to be one
        assert_eq!(read(b"\xff\xd8\xff\xe0 a jpeg".to_vec()), None);
        assert_eq!(read(heif_file(1, 1, &[2], false)[..100_050].to_vec()), None);
        let mut big = bx(b"meta", &[]);
        big[..4].copy_from_slice(&(2 * META_BYTES as u32).to_be_bytes());
        assert_eq!(read(big), None);
    }
}
