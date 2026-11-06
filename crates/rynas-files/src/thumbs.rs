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
        return std::fs::read(p).ok();
    }
    let name = file.file_name()?.to_string_lossy().to_lowercase();
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    if !makes_thumbnail(ext) {
        return None;
    }
    let data = ffmpeg(file, if big { BIG } else { SMALL }, is_video(ext))?;
    if let Some(p) = cached {
        store(&p, &data);
    }
    Some(data)
}

/// `<cache>/<2 hex>/<64 hex>.jpg`
fn cache_path(dir: &Path, meta: &std::fs::Metadata, big: bool) -> PathBuf {
    let mut h = Sha256::new();
    for n in [meta.dev(), meta.ino(), meta.size(), meta.mtime() as u64, meta.mtime_nsec() as u64, u64::from(big)] {
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
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
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
    std::fs::read_dir(dir.join("@eaDir")).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default()
}

/// One JPEG frame scaled to `width` (never enlarged), on stdout.
fn ffmpeg(file: &Path, width: u32, video: bool) -> Option<Vec<u8>> {
    if !Path::new(FFMPEG).exists() {
        return None;
    }
    let mut input = std::ffi::OsString::from("file:");
    input.push(file.as_os_str());
    let mut cmd = Command::new(FFMPEG);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    if video {
        // past a black first frame
        cmd.args(["-ss", "2"]);
    }
    cmd.arg("-i")
        .arg(input)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!("scale='min({width},iw)':-2"))
        .args(["-q:v", "5", "-f", "image2", "-c:v", "mjpeg", "pipe:1"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.by_ref().take(MAX_BYTES + 1).read_to_end(&mut s);
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
    let data = reader.join().ok()?;
    // a short video has no frame at 2 s: try its first one
    if video && (!ok || data.is_empty()) {
        return ffmpeg_first(file, width);
    }
    (ok && !data.is_empty() && data.len() as u64 <= MAX_BYTES && data.starts_with(&[0xff, 0xd8])).then_some(data)
}

fn ffmpeg_first(file: &Path, width: u32) -> Option<Vec<u8>> {
    ffmpeg(file, width, false)
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
}
