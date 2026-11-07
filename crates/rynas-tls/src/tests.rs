use super::*;

const T0: i64 = 1_800_000_000_000;

fn addrs() -> Vec<IpAddr> {
    vec![
        "192.0.2.10".parse().unwrap(),
        "2001:db8::10".parse().unwrap(),
        "fe80::1".parse().unwrap(),
        "169.254.7.7".parse().unwrap(),
        "::ffff:198.51.100.3".parse().unwrap(),
    ]
}

#[test]
fn names_cover_host_mdns_loopback_and_addresses() {
    let n = names("NAS", &addrs());
    assert_eq!(
        n.dns.iter().map(String::as_str).collect::<Vec<_>>(),
        ["localhost", "nas", "nas.local"]
    );
    let ips: Vec<String> = n.ips.iter().map(|i| i.to_string()).collect();
    assert!(ips.contains(&"127.0.0.1".to_string()));
    assert!(ips.contains(&"::1".to_string()));
    assert!(ips.contains(&"192.0.2.10".to_string()));
    assert!(ips.contains(&"2001:db8::10".to_string()));
    assert!(ips.contains(&"169.254.7.7".to_string()));
    // mapped IPv4 as IPv4; link-local IPv6 never (no zone ids in URLs)
    assert!(ips.contains(&"198.51.100.3".to_string()));
    assert!(!ips.iter().any(|i| i.starts_with("fe80")));
}

#[test]
fn fqdn_hostname_adds_short_name_and_mdns_of_first_label() {
    let n = names("nas.example.lan", &[]);
    assert!(n.dns.contains("nas.example.lan"));
    assert!(n.dns.contains("nas"));
    assert!(n.dns.contains("nas.local"));
    // an unusable host name still gives localhost
    let bad = names("bad_name!", &[]);
    assert_eq!(bad.dns.len(), 1);
}

#[test]
fn leaf_from_local_ca_has_every_name_and_verifies() {
    let ca = generate_ca("nas", T0).unwrap();
    let want = names("nas", &addrs());
    let leaf = issue_leaf(&ca, &want, T0).unwrap();
    let chain = inspect(&leaf.chain).unwrap();
    assert_eq!(chain.len(), 2, "leaf + local CA");
    let (l, c) = (&chain[0], &chain[1]);
    assert!(c.is_ca && c.self_signed);
    assert!(!l.is_ca && !l.self_signed);
    assert_eq!(l.issuer, c.subject);
    assert_eq!(l.subject, "CN=nas");
    assert_eq!(l.key_type, "ECDSA P-256");
    assert!(missing_names(l, &want).is_empty());
    assert_eq!(l.not_after_ms - l.not_before_ms, (LEAF_DAYS + 1) * DAY_MS);
    assert_eq!(c.not_after_ms - c.not_before_ms, (CA_DAYS + 1) * DAY_MS);
    assert_eq!(l.fingerprint_sha256.len(), 32 * 3 - 1);
    // the pair loads in rustls and the key matches
    certified_key(&leaf).unwrap();
    // and it passes the import checks too (signature chain, serverAuth)
    let imp = validate_import(&leaf.chain, &leaf.key, T0).unwrap();
    assert_eq!(imp.chain, chain);
    assert!(warnings(&chain, &[], T0).is_empty());
}

#[test]
fn reissue_when_names_are_missing_or_expiry_is_near() {
    let ca = generate_ca("nas", T0).unwrap();
    let want = names("nas", &addrs());
    let leaf = inspect(&issue_leaf(&ca, &want, T0).unwrap().chain)
        .unwrap()
        .remove(0);
    assert!(!needs_reissue(&leaf, &want, T0));
    // an address that went away does not matter, a new one does
    let fewer = names("nas", &addrs()[..1]);
    assert!(!needs_reissue(&leaf, &fewer, T0));
    let more = names("nas", &["192.0.2.99".parse().unwrap()]);
    assert!(needs_reissue(&leaf, &more, T0));
    assert_eq!(missing_names(&leaf, &more), ["192.0.2.99"]);
    // new host name
    let renamed = names("vault", &addrs());
    assert_eq!(missing_names(&leaf, &renamed), ["vault", "vault.local"]);
    // 30 days before the end
    let late = leaf.not_after_ms - (RENEW_DAYS - 1) * DAY_MS;
    assert!(needs_reissue(&leaf, &want, late));
    assert_eq!(
        warnings(std::slice::from_ref(&leaf), &[], late)[0],
        TlsWarning::ExpiresSoon
    );
    assert_eq!(
        warnings(std::slice::from_ref(&leaf), &[], leaf.not_after_ms)[0],
        TlsWarning::Expired
    );
}

#[test]
fn wildcard_covers_one_label() {
    let leaf = TlsCertificate {
        dns_names: vec!["*.example.com".into()],
        ..Default::default()
    };
    let mut want = Names::default();
    want.dns.insert("nas.example.com".into());
    assert!(missing_names(&leaf, &want).is_empty());
    want.dns.insert("example.com".into());
    want.dns.insert("a.b.example.com".into());
    assert_eq!(
        missing_names(&leaf, &want),
        ["a.b.example.com", "example.com"]
    );
}

#[test]
fn import_refuses_a_key_of_another_certificate() {
    let ca = generate_ca("nas", T0).unwrap();
    let want = names("nas", &[]);
    let a = issue_leaf(&ca, &want, T0).unwrap();
    let b = issue_leaf(&ca, &want, T0).unwrap();
    let e = validate_import(&a.chain, &b.key, T0).unwrap_err();
    assert_eq!(e.field, "private_key");
    assert!(e.message.contains("does not belong"), "{e}");
    assert!(
        certified_key(&PemPair {
            chain: a.chain,
            key: b.key
        })
        .is_err()
    );
}

#[test]
fn import_checks_order_expiry_and_garbage() {
    let ca = generate_ca("nas", T0).unwrap();
    let leaf = issue_leaf(&ca, &names("nas", &[]), T0).unwrap();
    // CA first
    let swapped = format!("{}{}", ca.chain, leaf.chain);
    let e = validate_import(&swapped, &leaf.key, T0).unwrap_err();
    assert_eq!(e.field, "certificate");
    // a chain whose second certificate did not sign the first
    let other_ca = generate_ca("other", T0).unwrap();
    let first_only = &leaf.chain[..leaf.chain.find("-----END CERTIFICATE-----").unwrap() + 26];
    let broken = format!("{first_only}{}", other_ca.chain);
    let e = validate_import(&broken, &leaf.key, T0).unwrap_err();
    assert!(e.message.contains("not signed by"), "{e}");
    // expired / not yet valid
    let late = T0 + (LEAF_DAYS + 2) * DAY_MS;
    assert!(
        validate_import(&leaf.chain, &leaf.key, late)
            .unwrap_err()
            .message
            .contains("expired")
    );
    let early = T0 - 3 * DAY_MS;
    assert!(
        validate_import(&leaf.chain, &leaf.key, early)
            .unwrap_err()
            .message
            .contains("not valid yet")
    );
    // garbage
    assert_eq!(
        validate_import("hello", &leaf.key, T0).unwrap_err().field,
        "certificate"
    );
    assert_eq!(
        validate_import(&leaf.chain, "hello", T0).unwrap_err().field,
        "private_key"
    );
    let enc = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n";
    assert!(
        validate_import(&leaf.chain, enc, T0)
            .unwrap_err()
            .message
            .contains("passphrase")
    );
    // the leaf alone (no intermediates) imports, with a warning
    let alone = validate_import(first_only, &leaf.key, T0).unwrap();
    assert_eq!(
        warnings(&alone.chain, &[], T0),
        [TlsWarning::ChainIncomplete]
    );
}

#[test]
fn import_normalises_pem_text() {
    let ca = generate_ca("nas", T0).unwrap();
    let leaf = issue_leaf(&ca, &names("nas", &[]), T0).unwrap();
    let noisy = format!(
        "subject=CN = nas\r\n{}\r\nBag Attributes\n",
        leaf.chain.replace('\n', "\r\n")
    );
    let imp = validate_import(&noisy, &leaf.key, T0).unwrap();
    assert_eq!(imp.pair.chain, leaf.chain);
}
