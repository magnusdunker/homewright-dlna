//! SSDP search for media renderers. The search goes out on every IPv4
//! interface, because a socket bound to 0.0.0.0 sends multicast on only one
//! of them, and on a PC with WSL or a VPN that is often the wrong one.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;

pub const TARGET: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const MULTICAST: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

#[derive(Clone, Debug, PartialEq)]
pub struct Sighting {
    pub location: String,
    /// The UDN without `uuid:`, lowercase.
    pub udn: String,
    pub server: String,
}

fn m_search(st: &str, mx: u64) -> String {
    format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {st}\r\n\r\n")
}

/// A search response, if it is a media renderer.
pub fn parse_response(text: &str) -> Option<Sighting> {
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    if !first.starts_with("HTTP/") && !first.starts_with("NOTIFY") {
        return None;
    }
    let mut location = String::new();
    let mut usn = String::new();
    let mut st = String::new();
    let mut server = String::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let v = v.trim().to_string();
        match k.trim().to_lowercase().as_str() {
            "location" => location = v,
            "usn" => usn = v,
            "st" | "nt" => st = v,
            "server" => server = v,
            _ => {}
        }
    }
    if location.is_empty() || !(st.contains(":MediaRenderer:") || usn.contains(":MediaRenderer:")) {
        return None;
    }
    let udn = usn.split("::").next().unwrap_or_default().trim_start_matches("uuid:").to_lowercase();
    if udn.is_empty() {
        return None;
    }
    Some(Sighting { location, udn, server })
}

fn interfaces() -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(a) if !a.is_link_local() => Some(a),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        out.push(Ipv4Addr::UNSPECIFIED);
    }
    out
}

fn socket_on(ip: Ipv4Addr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.bind(&SocketAddr::from((ip, 0)).into())?;
    if !ip.is_unspecified() {
        s.set_multicast_if_v4(&ip)?;
    }
    s.set_multicast_ttl_v4(2)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

/// Search for `wait` and return each renderer once.
pub async fn search(wait: Duration) -> Vec<Sighting> {
    let mut tasks = tokio::task::JoinSet::new();
    for ip in interfaces() {
        tasks.spawn(async move {
            let mut found = vec![];
            let Ok(sock) = socket_on(ip) else { return found };
            let msg = m_search(TARGET, wait.as_secs().clamp(1, 5));
            for _ in 0..2 {
                let _ = sock.send_to(msg.as_bytes(), MULTICAST).await;
            }
            let deadline = tokio::time::Instant::now() + wait;
            let mut buf = vec![0u8; 4096];
            while let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
                if let Some(s) = parse_response(&String::from_utf8_lossy(&buf[..n])) {
                    found.push(s);
                }
            }
            found
        });
    }
    let mut out: Vec<Sighting> = vec![];
    while let Some(r) = tasks.join_next().await {
        for s in r.unwrap_or_default() {
            if !out.iter().any(|o| o.udn == s.udn) {
                out.push(s);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_response() {
        let r = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: http://192.168.0.234:9197/dmr\r\nSERVER: Samsung-Linux/4.1, UPnP/1.0, Samsung_UPnP_SDK/1.0\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:E6822B86-2c69-4f14-886d-68dc7d52f380::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        let s = parse_response(r).unwrap();
        assert_eq!(s.location, "http://192.168.0.234:9197/dmr");
        assert_eq!(s.udn, "e6822b86-2c69-4f14-886d-68dc7d52f380");
        let other = r.replace("device:MediaRenderer", "device:MediaServer");
        assert!(parse_response(&other).is_none());
    }
}
