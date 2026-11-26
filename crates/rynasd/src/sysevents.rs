//! The one watcher of the system's storage events: whoever needs to know that disks,
//! partitions, arrays or mounts changed subscribes here instead of looking every few
//! seconds (`docs/phase1/tasks.md` "Events, not loops").
//!
//! One thread (`sysevents`) sleeps in `poll(2)` on three descriptors and wakes only when
//! the kernel has something to say:
//!
//! - **udev netlink** (`NETLINK_KOBJECT_UEVENT`): `add`/`remove`/`change` of the `block`
//!   subsystem. udevd's group when udevd runs (the event comes after its rules: device
//!   links and the udev database are ready), the kernel's group otherwise (containers).
//!   A dropped burst (`ENOBUFS`) is passed on as an unknown block change.
//! - **`/proc/self/mountinfo`**: the kernel signals `POLLPRI` on every change of the mount
//!   table (reading the table to its end arms the next one).
//! - **`/proc/mdstat`**: `POLLPRI` on every md event (array started, degraded, resync
//!   finished).
//!
//! Events go out on a broadcast channel ([`subscribe`]); a subscriber that lags gets
//! `Lagged` and must treat it as "everything may have changed". [`next_burst`] turns a
//! burst (a disk with five partitions, a container start) into one wake-up.
//! [`storage_changes`] is the debounced "the storage overview may have changed" of the
//! task stream (`storage_changed`, forwarded by rynas-web as `storage.changed`): block and
//! md events, and mount changes only when the set of block-device mounts changed (a
//! container's overlay mount is not a storage change).
//!
//! Nothing here runs on a timer. Subscribers that cannot trust events alone (a fake
//! `--hw-root`, a kernel without the netlink socket) keep a slow safety fallback of their
//! own, never a fast loop.

use std::io::{Read, Seek};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::broadcast;

/// One event of the watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SysEvent {
    /// The mount table changed.
    Mounts,
    /// A block device came, went or changed; `None`: events were dropped (unknown which).
    Block(Option<BlockEvent>),
    /// An md array changed state.
    Md,
}

/// A uevent of the `block` subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockEvent {
    /// `add`, `remove`, `change`, `move`, `online`, `offline`, `bind`, `unbind`.
    pub action: String,
    /// Kernel name (`sda`, `sda1`, `md2`, `dm-0`).
    pub devname: String,
    /// `disk` or `partition`.
    pub devtype: String,
}

/// Capacity of the channel; a slower subscriber lags (and re-reads everything).
const CAPACITY: usize = 64;
/// udevd's netlink group, and the kernel's.
const GROUP_UDEV: u32 = 2;
const GROUP_KERNEL: u32 = 1;
/// How long a burst must be quiet before [`storage_changes`] says so.
const STORAGE_SETTLE: Duration = Duration::from_secs(2);

static BUS: OnceLock<broadcast::Sender<SysEvent>> = OnceLock::new();
static STORAGE: OnceLock<broadcast::Sender<()>> = OnceLock::new();

/// The events from now on; the watcher thread starts with the first subscriber.
pub fn subscribe() -> broadcast::Receiver<SysEvent> {
    BUS.get_or_init(|| {
        let (tx, _) = broadcast::channel(CAPACITY);
        let t = tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("sysevents".into())
            .stack_size(128 << 10)
            .spawn(move || watch(&t))
        {
            tracing::warn!("system events: no watcher thread: {e}");
        }
        tx
    })
    .subscribe()
}

/// Waits for the next event `wanted` accepts (a lag counts as one), then until no further
/// event came for `quiet`: a burst is one wake-up. `false` once the watcher is gone.
pub async fn next_burst(rx: &mut broadcast::Receiver<SysEvent>, quiet: Duration, mut wanted: impl FnMut(&SysEvent) -> bool) -> bool {
    loop {
        match rx.recv().await {
            Ok(e) if wanted(&e) => break,
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => break,
            Err(broadcast::error::RecvError::Closed) => return false,
        }
    }
    loop {
        match tokio::time::timeout(quiet, rx.recv()).await {
            Err(_) => return true,
            Ok(Err(broadcast::error::RecvError::Closed)) => return true,
            Ok(_) => {}
        }
    }
}

/// "The storage overview may have changed", debounced ([`STORAGE_SETTLE`]): disks,
/// partitions, arrays, or the block-device mounts. One task for every subscriber.
pub fn storage_changes() -> broadcast::Receiver<()> {
    STORAGE
        .get_or_init(|| {
            let (tx, _) = broadcast::channel(4);
            let t = tx.clone();
            let mut rx = subscribe();
            tokio::spawn(async move {
                let mut mounts = device_mounts(&read_mountinfo());
                loop {
                    let alive = next_burst(&mut rx, STORAGE_SETTLE, |e| match e {
                        SysEvent::Mounts => {
                            let now = device_mounts(&read_mountinfo());
                            let changed = now != mounts;
                            mounts = now;
                            changed
                        }
                        _ => true,
                    })
                    .await;
                    if !alive {
                        return;
                    }
                    // no subscriber is fine
                    let _ = t.send(());
                }
            });
            tx
        })
        .subscribe()
}

fn read_mountinfo() -> String {
    std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
}

/// The block-device mounts of a mount table (mount point, source, type), sorted: what the
/// storage overview shows of it.
pub fn device_mounts(mountinfo: &str) -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = mountinfo
        .lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let point = pre.split(' ').nth(4)?;
            let mut p = post.split(' ');
            let (fstype, source) = (p.next()?, p.next()?);
            source.starts_with("/dev/").then(|| (point.to_string(), source.to_string(), fstype.to_string()))
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The `block` uevent in one netlink message (udevd's format or the kernel's).
pub fn parse_uevent(msg: &[u8]) -> Option<BlockEvent> {
    let props: &[u8] = if msg.starts_with(b"libudev\0") {
        // struct udev_monitor_netlink_header: prefix[8], magic, header_size,
        // properties_off, properties_len (native-endian u32 but the magic)
        let word = |at: usize| msg.get(at..at + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as usize);
        let (off, len) = (word(16)?, word(20)?);
        msg.get(off..off.checked_add(len)?)?
    } else {
        // "action@devpath\0KEY=value\0..."
        let nul = msg.iter().position(|&b| b == 0)?;
        msg.get(nul + 1..)?
    };
    let mut ev = BlockEvent::default();
    let mut block = false;
    for kv in props.split(|&b| b == 0) {
        let Ok(kv) = std::str::from_utf8(kv) else { continue };
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "ACTION" => ev.action = v.to_string(),
            "SUBSYSTEM" => block = v == "block",
            "DEVNAME" => ev.devname = v.trim_start_matches("/dev/").to_string(),
            "DEVTYPE" => ev.devtype = v.to_string(),
            _ => {}
        }
    }
    (block && !ev.action.is_empty()).then_some(ev)
}

fn open_uevent(group: u32) -> std::io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the descriptor is owned right after
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nobody else owns
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: a zeroed sockaddr_nl is a valid value
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    sa.nl_groups = group;
    // SAFETY: bind(2) with a sockaddr_nl of the right size
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&sa as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // a bigger receive buffer for bursts (best effort)
    let size: libc::c_int = 1 << 20;
    // SAFETY: setsockopt(2) with an int option
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok(fd)
}

/// A `/proc` file that signals changes with `POLLPRI`, read to its end (which arms it).
struct PriFile {
    f: std::fs::File,
    buf: Vec<u8>,
}

impl PriFile {
    fn open(path: &str) -> Option<Self> {
        let mut p = Self { f: std::fs::File::open(path).ok()?, buf: Vec::new() };
        p.rearm().then_some(p)
    }

    fn rearm(&mut self) -> bool {
        self.buf.clear();
        self.f.rewind().is_ok() && self.f.read_to_end(&mut self.buf).is_ok()
    }
}

fn watch(tx: &broadcast::Sender<SysEvent>) {
    let mut mounts = PriFile::open("/proc/self/mountinfo");
    let mut mdstat = PriFile::open("/proc/mdstat");
    let group = if std::path::Path::new("/run/udev/control").exists() { GROUP_UDEV } else { GROUP_KERNEL };
    let uevent = open_uevent(group).map_err(|e| tracing::warn!("system events: no uevent socket: {e}")).ok();
    let mut buf = vec![0u8; 16 << 10];
    loop {
        let mut pfds = Vec::with_capacity(3);
        let mut kinds = Vec::with_capacity(3);
        if let Some(m) = &mounts {
            pfds.push(libc::pollfd { fd: m.f.as_raw_fd(), events: libc::POLLPRI | libc::POLLERR, revents: 0 });
            kinds.push(0);
        }
        if let Some(m) = &mdstat {
            pfds.push(libc::pollfd { fd: m.f.as_raw_fd(), events: libc::POLLPRI | libc::POLLERR, revents: 0 });
            kinds.push(1);
        }
        if let Some(u) = &uevent {
            pfds.push(libc::pollfd { fd: u.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            kinds.push(2);
        }
        if pfds.is_empty() {
            tracing::warn!("system events: nothing to watch");
            return;
        }
        // SAFETY: poll(2) on descriptors that live as long as this call, no timeout
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            tracing::warn!("system events: poll: {}", std::io::Error::last_os_error());
            return;
        }
        for (p, kind) in pfds.iter().zip(&kinds) {
            if p.revents == 0 {
                continue;
            }
            let ev = match kind {
                0 => {
                    if mounts.as_mut().is_some_and(|m| !m.rearm()) {
                        mounts = None;
                    }
                    vec![SysEvent::Mounts]
                }
                1 => {
                    if mdstat.as_mut().is_some_and(|m| !m.rearm()) {
                        mdstat = None;
                    }
                    vec![SysEvent::Md]
                }
                _ => uevent.as_ref().map(|u| drain_uevents(u, &mut buf)).unwrap_or_default(),
            };
            for e in ev {
                // no subscriber is fine
                let _ = tx.send(e);
            }
        }
    }
}

/// Every message waiting on the uevent socket, as block events.
fn drain_uevents(fd: &OwnedFd, buf: &mut [u8]) -> Vec<SysEvent> {
    let mut out = Vec::new();
    loop {
        // SAFETY: recv(2) into a buffer of the given length
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if n < 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                // the kernel dropped messages: something changed, unknown what
                Some(libc::ENOBUFS) => {
                    out.push(SysEvent::Block(None));
                    continue;
                }
                _ => break,
            }
        }
        if n == 0 {
            break;
        }
        if let Some(e) = parse_uevent(&buf[..n as usize]) {
            out.push(SysEvent::Block(Some(e)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_and_udev_messages() {
        let k = b"add@/devices/pci0000:00/usb1/1-1/host6/target6:0:0/6:0:0:0/block/sdc\0ACTION=add\0DEVPATH=/devices/x/block/sdc\0SUBSYSTEM=block\0MAJOR=8\0MINOR=32\0DEVNAME=sdc\0DEVTYPE=disk\0SEQNUM=4242\0";
        assert_eq!(parse_uevent(k), Some(BlockEvent { action: "add".into(), devname: "sdc".into(), devtype: "disk".into() }));
        let net = b"add@/devices/virtual/net/veth1\0ACTION=add\0SUBSYSTEM=net\0INTERFACE=veth1\0";
        assert_eq!(parse_uevent(net), None);
        // udevd: header, then the properties at properties_off
        let props = b"ACTION=remove\0SUBSYSTEM=block\0DEVNAME=/dev/sdc1\0DEVTYPE=partition\0";
        let mut m = b"libudev\0".to_vec();
        m.extend_from_slice(&0xfeedcafe_u32.to_be_bytes());
        m.extend_from_slice(&40u32.to_ne_bytes());
        m.extend_from_slice(&40u32.to_ne_bytes());
        m.extend_from_slice(&(props.len() as u32).to_ne_bytes());
        m.resize(40, 0);
        m.extend_from_slice(props);
        assert_eq!(parse_uevent(&m), Some(BlockEvent { action: "remove".into(), devname: "sdc1".into(), devtype: "partition".into() }));
        // a header pointing past the message
        let mut bad = m.clone();
        bad[20..24].copy_from_slice(&9999u32.to_ne_bytes());
        assert_eq!(parse_uevent(&bad), None);
        assert_eq!(parse_uevent(b""), None);
    }

    #[test]
    fn only_block_device_mounts_count() {
        let a = "36 25 0:32 / /srv/vol1 rw,relatime shared:1 - btrfs /dev/md2 rw\n\
                 40 25 0:40 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw\n\
                 41 25 0:41 / /run/user/1000 rw - tmpfs tmpfs rw\n";
        let b = format!("{a}42 25 0:42 / /var/lib/docker/overlay2/y/merged rw - overlay overlay rw\n");
        assert_eq!(device_mounts(a), device_mounts(&b));
        assert_eq!(device_mounts(a), vec![("/srv/vol1".into(), "/dev/md2".into(), "btrfs".into())]);
        let c = format!("{a}43 25 8:33 / /media/usb rw - exfat /dev/sdc1 rw\n");
        assert_ne!(device_mounts(a), device_mounts(&c));
    }

    #[tokio::test]
    async fn a_burst_is_one_wakeup() {
        let (tx, mut rx) = broadcast::channel(16);
        tokio::spawn(async move {
            for _ in 0..5 {
                let _ = tx.send(SysEvent::Md);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = tx.send(SysEvent::Mounts);
            tokio::time::sleep(Duration::from_millis(400)).await;
        });
        assert!(next_burst(&mut rx, Duration::from_millis(150), |_| true).await);
        // the five md events were one; the mounts event is the next burst
        assert!(next_burst(&mut rx, Duration::from_millis(150), |e| *e == SysEvent::Mounts).await);
        assert!(!next_burst(&mut rx, Duration::from_millis(150), |_| true).await);
    }
}
