//! A minimal DNS client (RFC 1035 over UDP): one A or AAAA question to one server, the
//! answer's addresses and response code. Enough to compare what the LAN resolver and a public
//! resolver say about a name (rebind detection) without the system resolver's caches and
//! search domains in between.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use crate::Rtype;

/// What a server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// NOERROR with addresses of the asked type (CNAME chains followed by the server).
    Addrs(Vec<IpAddr>),
    /// NOERROR without an address of that type.
    NoData,
    /// NXDOMAIN.
    NxDomain,
    /// REFUSED.
    Refused,
    /// SERVFAIL.
    ServFail,
    /// Another response code.
    Other(u8),
}

impl Answer {
    pub fn addrs(&self) -> &[IpAddr] {
        match self {
            Answer::Addrs(a) => a,
            _ => &[],
        }
    }

    /// Short English text (`192.0.2.10`, `NXDOMAIN`...).
    pub fn describe(&self) -> String {
        match self {
            Answer::Addrs(a) => a
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            Answer::NoData => "no address".into(),
            Answer::NxDomain => "NXDOMAIN (no such name)".into(),
            Answer::Refused => "REFUSED".into(),
            Answer::ServFail => "SERVFAIL".into(),
            Answer::Other(c) => format!("response code {c}"),
        }
    }
}

fn qtype(t: Rtype) -> u16 {
    match t {
        Rtype::A => 1,
        Rtype::Aaaa => 28,
    }
}

/// The query packet: recursion desired, one question, class IN.
pub fn build_query(id: u16, name: &str, t: Rtype) -> Result<Vec<u8>, String> {
    let mut p = Vec::with_capacity(32 + name.len());
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&[0x01, 0x00]); // RD
    p.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // QD=1
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("{name} is not a valid name"));
        }
        p.push(label.len() as u8);
        p.extend_from_slice(label.as_bytes());
    }
    p.push(0);
    p.extend_from_slice(&qtype(t).to_be_bytes());
    p.extend_from_slice(&[0, 1]);
    Ok(p)
}

/// Skips a (possibly compressed) name at `i`; returns the offset after it.
fn skip_name(b: &[u8], mut i: usize) -> Result<usize, String> {
    for _ in 0..128 {
        let len = *b.get(i).ok_or("truncated name")? as usize;
        if len & 0xc0 == 0xc0 {
            return Ok(i + 2);
        }
        if len == 0 {
            return Ok(i + 1);
        }
        i += 1 + len;
    }
    Err("name too long".into())
}

fn u16_at(b: &[u8], i: usize) -> Result<u16, String> {
    Ok(u16::from_be_bytes([
        *b.get(i).ok_or("truncated")?,
        *b.get(i + 1).ok_or("truncated")?,
    ]))
}

/// Reads the answer to query `id` of type `t`.
pub fn parse_response(id: u16, t: Rtype, b: &[u8]) -> Result<Answer, String> {
    if b.len() < 12 {
        return Err("short answer".into());
    }
    if u16_at(b, 0)? != id {
        return Err("answer to another query".into());
    }
    if b[2] & 0x80 == 0 {
        return Err("not an answer".into());
    }
    let rcode = b[3] & 0x0f;
    match rcode {
        0 => {}
        2 => return Ok(Answer::ServFail),
        3 => return Ok(Answer::NxDomain),
        5 => return Ok(Answer::Refused),
        c => return Ok(Answer::Other(c)),
    }
    let qd = u16_at(b, 4)? as usize;
    let an = u16_at(b, 6)? as usize;
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(b, i)? + 4;
    }
    let mut addrs = Vec::new();
    for _ in 0..an {
        i = skip_name(b, i)?;
        let rt = u16_at(b, i)?;
        let len = u16_at(b, i + 8)? as usize;
        let data = b.get(i + 10..i + 10 + len).ok_or("truncated record")?;
        if rt == qtype(t) {
            match (t, len) {
                (Rtype::A, 4) => addrs.push(IpAddr::V4(Ipv4Addr::new(
                    data[0], data[1], data[2], data[3],
                ))),
                (Rtype::Aaaa, 16) => {
                    let mut o = [0u8; 16];
                    o.copy_from_slice(data);
                    addrs.push(IpAddr::V6(Ipv6Addr::from(o)));
                }
                _ => {}
            }
        }
        i += 10 + len;
    }
    Ok(if addrs.is_empty() {
        Answer::NoData
    } else {
        Answer::Addrs(addrs)
    })
}

/// Asks `server` for the `t` records of `name` (two tries within `timeout`).
pub fn query(
    server: SocketAddr,
    name: &str,
    t: Rtype,
    timeout: Duration,
) -> Result<Answer, String> {
    let mut idb = [0u8; 2];
    getrandom::fill(&mut idb).map_err(|e| e.to_string())?;
    let id = u16::from_ne_bytes(idb);
    let q = build_query(id, name, t)?;
    let bind: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0".parse().expect("valid")
    } else {
        "[::]:0".parse().expect("valid")
    };
    let sock = UdpSocket::bind(bind).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(timeout / 2))
        .map_err(|e| e.to_string())?;
    sock.connect(server).map_err(|e| format!("{server}: {e}"))?;
    let mut buf = [0u8; 1500];
    let mut stray = 0;
    for attempt in 0..2 {
        sock.send(&q).map_err(|e| format!("{server}: {e}"))?;
        loop {
            match sock.recv(&mut buf) {
                Ok(n) => match parse_response(id, t, &buf[..n]) {
                    Ok(a) => return Ok(a),
                    // a stray or late packet: keep waiting for ours
                    Err(_) => stray += 1,
                },
                Err(e) if attempt == 1 => {
                    return Err(format!(
                        "{server}: no answer ({e}; {stray} unreadable packets)"
                    ));
                }
                Err(_) => break,
            }
        }
    }
    Err(format!("{server}: no answer"))
}

/// The `nameserver` lines of a `resolv.conf` text (port 53).
pub fn nameservers(resolv_conf: &str) -> Vec<SocketAddr> {
    resolv_conf
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("nameserver")).then(|| w.next()).flatten()
        })
        .filter_map(|a| a.split('%').next()?.parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer packet for `q` with the given records `(type, rdata)` and code.
    pub(crate) fn answer(q: &[u8], rcode: u8, records: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut p = q.to_vec();
        p[2] = 0x81;
        p[3] = 0x80 | rcode;
        p[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
        for (t, data) in records {
            p.extend_from_slice(&[0xc0, 12]); // pointer to the question name
            p.extend_from_slice(&t.to_be_bytes());
            p.extend_from_slice(&[0, 1, 0, 0, 1, 44]);
            p.extend_from_slice(&(data.len() as u16).to_be_bytes());
            p.extend_from_slice(data);
        }
        p
    }

    #[test]
    fn query_and_answers() {
        let q = build_query(0x1234, "nas.example.com", Rtype::A).unwrap();
        assert_eq!(&q[12..16], b"\x03nas");
        // a CNAME first, then the address
        let cname = vec![3, b'w', b'w', b'w', 0xc0, 12];
        let a = answer(&q, 0, &[(5, cname), (1, vec![192, 0, 2, 10])]);
        assert_eq!(
            parse_response(0x1234, Rtype::A, &a).unwrap(),
            Answer::Addrs(vec!["192.0.2.10".parse().unwrap()])
        );
        assert_eq!(
            parse_response(0x1234, Rtype::A, &answer(&q, 3, &[])).unwrap(),
            Answer::NxDomain
        );
        assert_eq!(
            parse_response(0x1234, Rtype::A, &answer(&q, 5, &[])).unwrap(),
            Answer::Refused
        );
        assert_eq!(
            parse_response(0x1234, Rtype::A, &answer(&q, 0, &[])).unwrap(),
            Answer::NoData
        );
        assert!(parse_response(0x9999, Rtype::A, &a).is_err());
        assert!(parse_response(0x1234, Rtype::A, &a[..20]).is_err());
        let q6 = build_query(7, "nas.example.com", Rtype::Aaaa).unwrap();
        let mut v6 = vec![0u8; 16];
        v6[0] = 0xfd;
        v6[15] = 0x10;
        assert_eq!(
            parse_response(7, Rtype::Aaaa, &answer(&q6, 0, &[(28, v6)])).unwrap(),
            Answer::Addrs(vec!["fd00::10".parse().unwrap()])
        );
        assert!(build_query(1, "a..b", Rtype::A).is_err());
    }

    #[test]
    fn udp_round_trip_with_a_fake_server() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let reply = answer(&buf[..n], 0, &[(1, vec![10, 0, 0, 5])]);
            server.send_to(&reply, from).unwrap();
        });
        let a = query(addr, "nas.example.com", Rtype::A, Duration::from_secs(2)).unwrap();
        assert_eq!(a.addrs(), ["10.0.0.5".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn resolv_conf() {
        let text =
            "# x\nsearch lan\nnameserver 192.0.2.1\nnameserver fe80::1%eth0\nnameserver nope\n";
        assert_eq!(
            nameservers(text),
            vec![
                "192.0.2.1:53".parse::<SocketAddr>().unwrap(),
                "[fe80::1]:53".parse().unwrap()
            ]
        );
    }
}
