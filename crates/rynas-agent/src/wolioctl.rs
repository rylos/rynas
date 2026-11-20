//! Wake-on-LAN of a card straight from the kernel (`SIOCETHTOOL`, `ETHTOOL_GWOL` and
//! `ETHTOOL_SWOL`), for PCs without the `ethtool` program (not installed by default on every
//! distribution).

const SIOCETHTOOL: libc::c_ulong = 0x8946;
const ETHTOOL_GWOL: u32 = 0x0000_0005;
const ETHTOOL_SWOL: u32 = 0x0000_0006;
/// `WAKE_MAGIC` of `linux/ethtool.h` (the `g` of `ethtool`).
const WAKE_MAGIC: u32 = 1 << 5;

#[repr(C)]
#[derive(Default)]
struct WolInfo {
    cmd: u32,
    supported: u32,
    wolopts: u32,
    sopass: [u8; 6],
}

#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    data: *mut libc::c_void,
    _pad: [u8; 16],
}

fn call(nic: &str, info: &mut WolInfo) -> Result<(), String> {
    if nic.is_empty() || nic.len() >= libc::IFNAMSIZ || nic.bytes().any(|b| b == 0) {
        return Err(format!("{nic}: not a card name"));
    }
    let mut req = IfReq { name: [0; libc::IFNAMSIZ], data: (info as *mut WolInfo).cast(), _pad: [0; 16] };
    for (d, s) in req.name.iter_mut().zip(nic.bytes()) {
        *d = s as libc::c_char;
    }
    // SAFETY: a datagram socket only to carry the ioctl; closed below
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: `req` points at a live `WolInfo` for the length of the call
    let r = unsafe { libc::ioctl(fd, SIOCETHTOOL as _, &mut req as *mut IfReq) };
    let err = std::io::Error::last_os_error();
    // SAFETY: our own descriptor
    unsafe { libc::close(fd) };
    if r < 0 { Err(format!("{nic}: {err}")) } else { Ok(()) }
}

/// (magic packet supported, magic packet enabled), `None` when the kernel does not say (no
/// such card, a driver without Wake-on-LAN support, no permission).
pub fn get(nic: &str) -> Option<(bool, bool)> {
    let mut info = WolInfo { cmd: ETHTOOL_GWOL, ..WolInfo::default() };
    call(nic, &mut info).ok()?;
    Some((info.supported & WAKE_MAGIC != 0, info.wolopts & WAKE_MAGIC != 0))
}

/// Wake on magic packet only, like `ethtool -s <nic> wol g` (needs `CAP_NET_ADMIN`).
pub fn enable_magic(nic: &str) -> Result<(), String> {
    let mut info = WolInfo { cmd: ETHTOOL_SWOL, wolopts: WAKE_MAGIC, ..WolInfo::default() };
    call(nic, &mut info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_refusals() {
        // struct ethtool_wolinfo: 3 × u32 + 6 bytes, padded to 4
        assert_eq!(std::mem::size_of::<WolInfo>(), 20);
        assert_eq!(get("no-such-card0"), None);
        assert!(enable_magic("").is_err());
        assert!(enable_magic("a-name-far-too-long-for-a-card").is_err());
        // the loopback has no Wake-on-LAN: the kernel says nothing usable or refuses
        assert!(get("lo").is_none_or(|(s, _)| !s));
    }
}
