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
    /// The watcher thread stopped (an error): no events until [`SysEvent::Restarted`].
    /// Subscribers that need to know fall back to their slow safety net ([`alive`]).
    Lost,
    /// The watcher thread runs again after a stop: everything may have changed meanwhile.
    Restarted,
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
/// The watcher thread is running and watching.
static ALIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Longest pause between two restarts of a watcher that keeps failing.
const RESTART_MAX: Duration = Duration::from_secs(60);

/// Whether events flow now: `false` before the first start, while the watcher thread is
/// down between restarts, or when it could not be started at all. A subscriber that relies
/// on events keeps a faster safety net of its own while this is `false`.
pub fn alive() -> bool {
    ALIVE.load(std::sync::atomic::Ordering::Acquire)
}

/// The events from now on; the watcher thread starts with the first subscriber and is
/// restarted (with a growing pause, at most [`RESTART_MAX`]) whenever it stops; each stop is
/// [`SysEvent::Lost`], each restart [`SysEvent::Restarted`].
pub fn subscribe() -> broadcast::Receiver<SysEvent> {
    BUS.get_or_init(|| {
        let (tx, _) = broadcast::channel(CAPACITY);
        let t = tx.clone();
        // the thread is marked alive before it runs: a first subscriber must not fall back
        ALIVE.store(true, std::sync::atomic::Ordering::Release);
        if let Err(e) = std::thread::Builder::new()
            .name("sysevents".into())
            .stack_size(128 << 10)
            .spawn(move || supervise(&t))
        {
            ALIVE.store(false, std::sync::atomic::Ordering::Release);
            tracing::warn!("system events: no watcher thread: {e}");
        }
        tx
    })
    .subscribe()
}

/// Runs [`watch`] again whenever it returns, with a pause that doubles up to
/// [`RESTART_MAX`] (back to one second after an hour of service).
fn supervise(tx: &broadcast::Sender<SysEvent>) {
    let mut pause = Duration::from_secs(1);
    let mut first = true;
    loop {
        let started = std::time::Instant::now();
        if !first {
            ALIVE.store(true, std::sync::atomic::Ordering::Release);
            let _ = tx.send(SysEvent::Restarted);
        }
        first = false;
        let why = watch(tx);
        ALIVE.store(false, std::sync::atomic::Ordering::Release);
        let _ = tx.send(SysEvent::Lost);
        if started.elapsed() > Duration::from_secs(3600) {
            pause = Duration::from_secs(1);
        }
        tracing::warn!("system events: watcher stopped ({why}), restarting in {} s", pause.as_secs());
        std::thread::sleep(pause);
        pause = (pause * 2).min(RESTART_MAX);
    }
}

/// Remembers the block-device mounts and says when they changed: a mount table change that
/// only adds a container's overlay is no storage change.
#[derive(Debug, Default)]
pub struct MountFilter(Vec<(String, String, String)>);

impl MountFilter {
    /// Starts from the mount table now.
    pub fn now() -> Self {
        Self(device_mounts(&read_mountinfo()))
    }

    /// Whether the block-device mounts of `mountinfo` differ from the last ones (kept).
    pub fn changed_with(&mut self, mountinfo: &str) -> bool {
        let now = device_mounts(mountinfo);
        let changed = now != self.0;
        self.0 = now;
        changed
    }

    /// [`MountFilter::changed_with`] the mount table now.
    pub fn changed(&mut self) -> bool {
        self.changed_with(&read_mountinfo())
    }
}

/// Waits for the next event `wanted` accepts (a lag or a restart of the watcher counts as
/// one), then until no further event came for `quiet`: a burst is one wake-up. `false` once
/// the channel is gone (never while the watcher only restarts).
pub async fn next_burst(rx: &mut broadcast::Receiver<SysEvent>, quiet: Duration, mut wanted: impl FnMut(&SysEvent) -> bool) -> bool {
    loop {
        match rx.recv().await {
            Ok(SysEvent::Restarted) => break,
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
                let mut mounts = MountFilter::now();
                loop {
                    let alive = next_burst(&mut rx, STORAGE_SETTLE, |e| match e {
                        SysEvent::Mounts => mounts.changed(),
                        SysEvent::Lost => false,
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
    // the sender's credentials with every message ([`sender_ok`])
    let on: libc::c_int = 1;
    // SAFETY: setsockopt(2) with an int option
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
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

/// Whether a netlink message comes from who may send it: in the kernel's group only the
/// kernel (port id 0); in udevd's group only root (udevd), by the credentials the kernel
/// attaches (`SO_PASSCRED`). A message without credentials is refused in udevd's group.
pub fn sender_ok(group: u32, nl_pid: u32, uid: Option<u32>) -> bool {
    match group {
        GROUP_KERNEL => nl_pid == 0 && uid.is_none_or(|u| u == 0),
        _ => uid == Some(0),
    }
}

/// Watches until something fails; returns why.
fn watch(tx: &broadcast::Sender<SysEvent>) -> String {
    let mut mounts = PriFile::open("/proc/self/mountinfo");
    let mut mdstat = PriFile::open("/proc/mdstat");
    let udevd = || std::path::Path::new("/run/udev/control").exists();
    let group = if udevd() { GROUP_UDEV } else { GROUP_KERNEL };
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
            return "nothing to watch".into();
        }
        // SAFETY: poll(2) on descriptors that live as long as this call, no timeout
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return format!("poll: {}", std::io::Error::last_os_error());
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
                _ => uevent.as_ref().map(|u| drain_uevents(u, group, &mut buf)).unwrap_or_default(),
            };
            for e in ev {
                // no subscriber is fine
                let _ = tx.send(e);
            }
            // udevd started after us: its group from now on (the events after its rules)
            if *kind == 2 && group == GROUP_KERNEL && udevd() {
                return "udevd started: switching to its events".into();
            }
        }
    }
}

/// Every message waiting on the uevent socket from an allowed sender ([`sender_ok`]), as
/// block events.
fn drain_uevents(fd: &OwnedFd, group: u32, buf: &mut [u8]) -> Vec<SysEvent> {
    let mut out = Vec::new();
    loop {
        // SAFETY: zeroed sockaddr_nl, iovec, msghdr and control buffer are valid values
        let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let mut ctrl = [0u64; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&mut sa as *mut libc::sockaddr_nl).cast();
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctrl.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&ctrl) as _;
        // SAFETY: recvmsg(2) into the buffers set up above
        let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut msg, libc::MSG_DONTWAIT) };
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
        let mut uid = None;
        // SAFETY: walking the control messages the kernel wrote into `ctrl`
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_CREDENTIALS {
                    let cred: libc::ucred = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast());
                    uid = Some(cred.uid);
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        if !sender_ok(group, sa.nl_pid, uid) {
            continue;
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

    #[test]
    fn only_the_kernel_or_root_may_send() {
        // kernel group: port id 0 only
        assert!(sender_ok(GROUP_KERNEL, 0, Some(0)));
        assert!(sender_ok(GROUP_KERNEL, 0, None));
        assert!(!sender_ok(GROUP_KERNEL, 4242, Some(0)));
        assert!(!sender_ok(GROUP_KERNEL, 0, Some(1000)));
        // udevd's group: root's credentials only
        assert!(sender_ok(GROUP_UDEV, 311, Some(0)));
        assert!(!sender_ok(GROUP_UDEV, 311, Some(1000)));
        assert!(!sender_ok(GROUP_UDEV, 311, None));
    }

    #[test]
    fn a_container_start_is_no_mount_change() {
        let base = "36 25 0:32 / /srv/vol1 rw - btrfs /dev/md2 rw\n";
        let mut f = MountFilter::default();
        assert!(f.changed_with(base), "first look: the mounts are new");
        let overlay = format!("{base}40 25 0:40 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw\n");
        assert!(!f.changed_with(&overlay));
        let usb = format!("{overlay}43 25 8:33 / /media/usb rw - exfat /dev/sdc1 rw\n");
        assert!(f.changed_with(&usb));
        assert!(!f.changed_with(&usb));
        assert!(f.changed_with(&overlay), "unplugged");
    }

    #[tokio::test]
    async fn a_restart_is_a_burst_and_lost_goes_to_the_filter() {
        let (tx, mut rx) = broadcast::channel(16);
        let _ = tx.send(SysEvent::Lost);
        let _ = tx.send(SysEvent::Restarted);
        // a filter that wants nothing still wakes on the restart (everything may have changed)
        let mut seen = Vec::new();
        assert!(next_burst(&mut rx, Duration::from_millis(50), |e| {
            seen.push(e.clone());
            false
        })
        .await);
        assert_eq!(seen, vec![SysEvent::Lost]);
        drop(tx);
        assert!(!next_burst(&mut rx, Duration::from_millis(50), |_| true).await);
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
