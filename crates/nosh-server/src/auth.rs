//! Who may call the local API without a session token (`automaticallyTrusted` in
//! `apps/noshd/src/server.ts`).

use hyper::HeaderMap;
use std::net::{Ipv4Addr, Ipv6Addr};

/// A request header as Node's `IncomingMessage.headers` holds it: repeats of the headers Node
/// keeps once are dropped, other repeats are joined with `, `. Bytes outside ASCII read as Latin-1.
pub(crate) fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    const SINGLE: [&str; 3] = ["authorization", "host", "content-type"];
    let latin1 = |value: &hyper::header::HeaderValue| -> String {
        value
            .as_bytes()
            .iter()
            .map(|&byte| char::from(byte))
            .collect()
    };
    let mut values = headers.get_all(name).iter();
    let first = latin1(values.next()?);
    if SINGLE.contains(&name) {
        return Some(first);
    }
    Some(values.fold(first, |joined, value| {
        format!("{joined}, {}", latin1(value))
    }))
}

/// `isLoopbackAddress`: 127.0.0.0/8, `::1`, and IPv4-mapped 127.0.0.0/8.
pub fn is_loopback_address(address: &str) -> bool {
    if let Ok(v4) = address.parse::<Ipv4Addr>() {
        return v4.octets()[0] == 127;
    }
    let Ok(v6) = address.parse::<Ipv6Addr>() else {
        return false;
    };
    let words = v6.segments();
    (words[..7].iter().all(|&word| word == 0) && words[7] == 1)
        || (words[..5].iter().all(|&word| word == 0) && words[5] == 0xffff && words[6] >> 8 == 127)
}

/// Requests from this machine that a browser page on another site could not have sent.
pub(crate) fn automatically_trusted(remote: &str, headers: &HeaderMap) -> bool {
    if !is_loopback_address(remote) {
        return false;
    }
    // DNS rebinding reaches loopback under an attacker-controlled Host name.
    let host = header(headers, "host");
    if !loopback_host(host.as_deref()) {
        return false;
    }
    let Some(origin) = header(headers, "origin") else {
        let fetch_site = header(headers, "sec-fetch-site");
        return fetch_site
            .is_none_or(|site| site.is_empty() || site == "same-origin" || site == "none");
    };
    let Some(host) = host.filter(|host| !host.is_empty()) else {
        return false;
    };
    match (
        url::Url::parse(&origin),
        url::Url::parse(&format!("http://{host}")),
    ) {
        (Ok(origin), Ok(own)) => {
            origin.origin().ascii_serialization() == own.origin().ascii_serialization()
        }
        _ => false,
    }
}

/// Whether the Host header names this machine: `localhost`, `[::1]`, or 127.0.0.0/8.
fn loopback_host(host: Option<&str>) -> bool {
    let Some(host) = host.filter(|host| !host.is_empty()) else {
        return false;
    };
    let Ok(url) = url::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    let name = url.host_str().unwrap_or_default();
    name == "localhost"
        || name == "[::1]"
        || name
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.octets()[0] == 127)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn trusts_same_origin_loopback_requests_only() {
        let host = ("host", "127.0.0.1:4321");
        assert!(automatically_trusted("127.0.0.1", &headers(&[host])));
        assert!(automatically_trusted("::ffff:127.0.0.1", &headers(&[host])));
        assert!(!automatically_trusted("192.168.1.10", &headers(&[host])));
        assert!(automatically_trusted(
            "127.0.0.1",
            &headers(&[host, ("origin", "http://127.0.0.1:4321")])
        ));
        assert!(!automatically_trusted(
            "127.0.0.1",
            &headers(&[host, ("origin", "http://127.0.0.1:9999")])
        ));
        assert!(!automatically_trusted(
            "127.0.0.1",
            &headers(&[host, ("origin", "null")])
        ));
        for site in ["same-origin", "none", ""] {
            assert!(automatically_trusted(
                "127.0.0.1",
                &headers(&[host, ("sec-fetch-site", site)])
            ));
        }
        assert!(!automatically_trusted(
            "127.0.0.1",
            &headers(&[host, ("sec-fetch-site", "same-site")])
        ));
        for name in ["localhost:1", "LOCALHOST", "[::1]:4321", "127.9.9.9"] {
            assert!(
                automatically_trusted("127.0.0.1", &headers(&[("host", name)])),
                "{name}"
            );
        }
        for name in ["attacker.invalid:4321", "128.0.0.1", "[::2]", ""] {
            assert!(
                !automatically_trusted("127.0.0.1", &headers(&[("host", name)])),
                "{name}"
            );
        }
        assert!(!automatically_trusted("127.0.0.1", &HeaderMap::new()));
    }

    #[test]
    fn joins_repeated_headers_as_node_does() {
        let map = headers(&[
            ("origin", "a"),
            ("origin", "b"),
            ("host", "x"),
            ("host", "y"),
        ]);
        assert_eq!(header(&map, "origin").as_deref(), Some("a, b"));
        assert_eq!(header(&map, "host").as_deref(), Some("x"));
        assert_eq!(header(&map, "missing"), None);
    }
}
