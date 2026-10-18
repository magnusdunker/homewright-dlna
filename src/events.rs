//! The HTTP listener that receives GENA events (NOTIFY) from the renderers.
//! Each subscription gets its own path, so an event that arrives before the
//! SUBSCRIBE answer, as the first one often does, still finds its device.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Notify {
    pub path: String,
    pub sid: String,
    pub body: String,
}

type Routes = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Notify>>>>;

pub struct EventServer {
    port: u16,
    routes: Routes,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for EventServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl EventServer {
    /// Listen on `port`, or on a free port when it is 0.
    pub async fn start(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
        let port = listener.local_addr()?.port();
        let routes: Routes = Arc::default();
        let r = routes.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let r = r.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(10), handle(stream, r)).await;
                });
            }
        });
        Ok(Self { port, routes, task })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn route(&self, path: &str, tx: mpsc::UnboundedSender<Notify>) {
        self.routes.lock().unwrap().insert(path.to_string(), tx);
    }

    pub fn unroute(&self, path: &str) {
        self.routes.lock().unwrap().remove(path);
    }
}

/// The address of this machine that `host` reaches it on, for the callback URL.
pub fn local_ip_towards(host: &str, port: u16) -> Option<IpAddr> {
    use std::net::ToSocketAddrs;
    let target = (host, port).to_socket_addrs().ok()?.find(SocketAddr::is_ipv4)?;
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(target).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

async fn handle(mut s: TcpStream, routes: Routes) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let mut body = buf[head_end..].to_vec();
    if let Some(len) = headers.get("content-length").and_then(|l| l.parse::<usize>().ok()) {
        while body.len() < len.min(1 << 20) {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
    } else if headers.get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        while dechunk(&body).is_none() && body.len() < 1 << 20 {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body = dechunk(&body).unwrap_or_default();
    }
    let tx = routes.lock().unwrap().get(&path).cloned();
    let status = match (method.as_str(), tx) {
        ("NOTIFY", Some(tx)) => {
            let sid = headers.get("sid").cloned().unwrap_or_default();
            let _ = tx.send(Notify { path: path.clone(), sid, body: String::from_utf8_lossy(&body).into_owned() });
            "200 OK"
        }
        ("NOTIFY", None) => "412 Precondition Failed",
        _ => "405 Method Not Allowed",
    };
    s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await?;
    s.shutdown().await
}

/// A complete chunked body, or `None` while the last chunk is missing.
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![];
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n")?;
        let size_text = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if data.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked() {
        assert_eq!(dechunk(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(), b"Wikipedia");
        assert!(dechunk(b"4\r\nWiki\r\n5\r\nped").is_none());
    }

    #[tokio::test]
    async fn routes_notify_by_path() {
        let server = EventServer::start(0).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        server.route("/a/AVTransport", tx);
        let mut s = TcpStream::connect(("127.0.0.1", server.port())).await.unwrap();
        s.write_all(b"NOTIFY /a/AVTransport HTTP/1.1\r\nSID: uuid:1\r\nNT: upnp:event\r\nContent-Length: 5\r\n\r\nhello").await.unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"));
        let n = rx.recv().await.unwrap();
        assert_eq!((n.sid.as_str(), n.body.as_str()), ("uuid:1", "hello"));

        let mut s = TcpStream::connect(("127.0.0.1", server.port())).await.unwrap();
        s.write_all(b"NOTIFY /b HTTP/1.1\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 412"));
    }
}
