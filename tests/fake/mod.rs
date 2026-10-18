//! A fake DLNA renderer on localhost: device and service descriptions, SOAP
//! control for AVTransport, RenderingControl and ConnectionManager, and GENA
//! subscriptions that send events back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use homewright_dlna::xml;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct State {
    pub transport: String,
    pub uri: String,
    pub meta: String,
    pub volume: u32,
    pub muted: bool,
    pub play_mode: String,
    pub position: String,
    /// every SOAP call: action and arguments
    pub calls: Vec<(String, HashMap<String, String>)>,
    /// (service kind, callback URL, SID)
    pub subs: Vec<(String, String, String)>,
    next_sid: u32,
    seq: u32,
}

pub struct FakeRenderer {
    pub port: u16,
    pub state: Arc<Mutex<State>>,
    /// While set, connections are closed without an answer.
    pub down: Arc<AtomicBool>,
    /// Whether subscriptions send events.
    events: bool,
    udn: String,
    renderer: bool,
}

const SINK: &str = "http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_FLAGS=ED100000000000000000000000000000,http-get:*:video/mp4:*";

impl FakeRenderer {
    pub async fn start(events: bool) -> Arc<Self> {
        Self::start_with("fake-renderer-1", events, true).await
    }

    /// `renderer: false` describes a media server instead.
    pub async fn start_with(udn: &str, events: bool, renderer: bool) -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = Arc::new(Self {
            port,
            state: Arc::new(Mutex::new(State {
                transport: "NO_MEDIA_PRESENT".into(),
                uri: String::new(),
                meta: String::new(),
                volume: 10,
                muted: false,
                play_mode: "NORMAL".into(),
                position: "0:00:00".into(),
                calls: vec![],
                subs: vec![],
                next_sid: 1,
                seq: 0,
            })),
            down: Arc::default(),
            events,
            udn: udn.into(),
            renderer,
        });
        let m = me.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let m = m.clone();
                tokio::spawn(async move {
                    if m.down.load(Ordering::SeqCst) {
                        drop(s);
                        return;
                    }
                    let _ = m.serve(s).await;
                });
            }
        });
        me
    }

    pub fn location(&self) -> String {
        format!("http://127.0.0.1:{}/desc.xml", self.port)
    }

    pub fn calls(&self, action: &str) -> Vec<HashMap<String, String>> {
        self.state.lock().unwrap().calls.iter().filter(|(a, _)| a == action).map(|(_, args)| args.clone()).collect()
    }

    /// Change the transport state as the remote control would, and tell the subscribers.
    pub async fn remote(&self, transport: &str) {
        self.state.lock().unwrap().transport = transport.into();
        self.notify("AVTransport").await;
    }

    fn description(&self) -> String {
        let services = if self.renderer {
            [("AVTransport", "avt"), ("RenderingControl", "rc"), ("ConnectionManager", "cm")]
                .iter()
                .map(|(k, p)| {
                    format!(
                        "<service><serviceType>urn:schemas-upnp-org:service:{k}:1</serviceType><serviceId>urn:upnp-org:serviceId:{k}</serviceId>\
                         <controlURL>/{p}/control</controlURL><eventSubURL>/{p}/event</eventSubURL><SCPDURL>/{p}.xml</SCPDURL></service>"
                    )
                })
                .collect::<String>()
        } else {
            "<service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType><controlURL>/cd/control</controlURL><eventSubURL>/cd/event</eventSubURL><SCPDURL>/cd.xml</SCPDURL></service>".into()
        };
        let kind = if self.renderer { "MediaRenderer" } else { "MediaServer" };
        format!(
            "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:df=\"http://schemas.microsoft.com/windows/2008/09/devicefoundation\">\
             <specVersion><major>1</major><minor>0</minor></specVersion><device>\
             <deviceType>urn:schemas-upnp-org:device:{kind}:1</deviceType><df:X_deviceCategory>Display.TV.LCD Multimedia.DMR</df:X_deviceCategory>\
             <friendlyName>Stue TV</friendlyName><manufacturer>Fake Electronics</manufacturer><modelName>FX-50</modelName>\
             <UDN>uuid:{}</UDN><serviceList>{services}</serviceList></device></root>",
            self.udn
        )
    }

    fn scpd(kind: &str) -> String {
        let (actions, vars) = match kind {
            "avt" => (
                vec!["SetAVTransportURI", "GetMediaInfo", "GetTransportInfo", "GetPositionInfo", "GetCurrentTransportActions", "GetTransportSettings", "Play", "Pause", "Stop", "Seek", "Next", "Previous", "SetPlayMode"],
                "<stateVariable sendEvents=\"no\"><name>CurrentPlayMode</name><dataType>string</dataType><allowedValueList>\
                 <allowedValue>NORMAL</allowedValue><allowedValue>SHUFFLE</allowedValue><allowedValue>REPEAT_ALL</allowedValue></allowedValueList></stateVariable>",
            ),
            "rc" => (
                vec!["GetVolume", "SetVolume", "GetMute", "SetMute"],
                "<stateVariable sendEvents=\"no\"><name>Volume</name><dataType>ui2</dataType><allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange></stateVariable>",
            ),
            _ => (vec!["GetProtocolInfo"], ""),
        };
        let actions: String = actions.iter().map(|a| format!("<action><name>{a}</name></action>")).collect();
        format!("<?xml version=\"1.0\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><actionList>{actions}</actionList><serviceStateTable>{vars}</serviceStateTable></scpd>")
    }

    fn transport_actions(st: &State) -> &'static str {
        match st.transport.as_str() {
            "PLAYING" => "Pause,Stop,Seek,Next,Previous",
            "PAUSED_PLAYBACK" => "Play,Stop,Seek",
            "STOPPED" => "Play,Seek",
            _ => "",
        }
    }

    /// The LastChange event for a service.
    fn last_change(&self, kind: &str) -> String {
        let st = self.state.lock().unwrap();
        let inner = if kind == "AVTransport" {
            format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\"><TransportState val=\"{}\"/><CurrentTrackURI val=\"{}\"/>\
                 <CurrentTrackMetaData val=\"{}\"/><CurrentTransportActions val=\"{}\"/><CurrentPlayMode val=\"{}\"/></InstanceID></Event>",
                st.transport,
                xml::escape(&st.uri),
                xml::escape(&st.meta),
                Self::transport_actions(&st),
                st.play_mode
            )
        } else {
            format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\"><InstanceID val=\"0\"><Volume channel=\"Master\" val=\"{}\"/><Mute channel=\"Master\" val=\"{}\"/></InstanceID></Event>",
                st.volume,
                u8::from(st.muted)
            )
        };
        format!(
            "<?xml version=\"1.0\"?><e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>",
            xml::escape(&inner)
        )
    }

    pub async fn notify(&self, kind: &str) {
        if !self.events {
            return;
        }
        let subs: Vec<(String, String)> = {
            let st = self.state.lock().unwrap();
            st.subs.iter().filter(|(k, _, _)| k == kind).map(|(_, cb, sid)| (cb.clone(), sid.clone())).collect()
        };
        for (cb, sid) in subs {
            let body = self.last_change(kind);
            let seq = {
                let mut st = self.state.lock().unwrap();
                st.seq += 1;
                st.seq
            };
            let url = reqwest::Url::parse(&cb).unwrap();
            let Ok(mut s) = TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).await else { continue };
            let req = format!(
                "NOTIFY {} HTTP/1.1\r\nHOST: {}:{}\r\nCONTENT-TYPE: text/xml\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\nContent-Length: {}\r\n\r\n{body}",
                url.path(),
                url.host_str().unwrap(),
                url.port().unwrap(),
                body.len()
            );
            let _ = s.write_all(req.as_bytes()).await;
            let mut answer = vec![];
            let _ = s.read_to_end(&mut answer).await;
        }
    }

    async fn serve(self: &Arc<Self>, mut s: TcpStream) -> std::io::Result<()> {
        let (method, path, headers, body) = read_request(&mut s).await?;
        let (status, extra, body) = match (method.as_str(), path.as_str()) {
            ("GET", "/desc.xml") => ("200 OK", String::new(), self.description()),
            ("GET", p) if p.ends_with(".xml") => ("200 OK", String::new(), Self::scpd(p.trim_start_matches('/').trim_end_matches(".xml"))),
            ("POST", p) if p.ends_with("/control") => {
                let action = headers.get("soapaction").and_then(|a| a.trim_matches('"').split('#').nth(1).map(String::from)).unwrap_or_default();
                let args: HashMap<String, String> = xml::parse(&body)
                    .and_then(|d| d.find("Body").and_then(|b| b.children.first().cloned()))
                    .map(|a| a.children.iter().map(|c| (c.name.clone(), c.text.clone())).collect())
                    .unwrap_or_default();
                let (out, changed) = self.action(&action, &args);
                let changed_kind = changed.map(String::from);
                let ns = match p {
                    "/avt/control" => "AVTransport",
                    "/rc/control" => "RenderingControl",
                    _ => "ConnectionManager",
                };
                let resp = match out {
                    Ok(out) => {
                        let inner: String = out.iter().map(|(k, v)| format!("<{k}>{}</{k}>", xml::escape(v))).collect();
                        ("200 OK", format!("<u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:{ns}:1\">{inner}</u:{action}Response>"))
                    }
                    Err(code) => (
                        "500 Internal Server Error",
                        format!("<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>Fejl</errorDescription></UPnPError></detail></s:Fault>"),
                    ),
                };
                let body = format!("<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>{}</s:Body></s:Envelope>", resp.1);
                if let Some(kind) = changed_kind {
                    let me = self.clone();
                    tokio::spawn(async move { me.notify(&kind).await });
                }
                (resp.0, String::new(), body)
            }
            ("SUBSCRIBE", p) => {
                let kind = if p.starts_with("/avt") { "AVTransport" } else { "RenderingControl" };
                let sid = match headers.get("sid") {
                    Some(sid) => sid.clone(),
                    None => {
                        let cb = headers.get("callback").map(|c| c.trim_matches(['<', '>']).to_string()).unwrap_or_default();
                        let mut st = self.state.lock().unwrap();
                        let sid = format!("uuid:sub-{}", st.next_sid);
                        st.next_sid += 1;
                        st.subs.push((kind.into(), cb, sid.clone()));
                        let me = self.clone();
                        let kind = kind.to_string();
                        // the initial event, right after the answer
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            me.notify(&kind).await
                        });
                        sid
                    }
                };
                ("200 OK", format!("SID: {sid}\r\nTIMEOUT: Second-300\r\n"), String::new())
            }
            ("UNSUBSCRIBE", _) => {
                if let Some(sid) = headers.get("sid") {
                    self.state.lock().unwrap().subs.retain(|(_, _, s)| s != sid);
                }
                ("200 OK", String::new(), String::new())
            }
            _ => ("404 Not Found", String::new(), String::new()),
        };
        let resp = format!("HTTP/1.1 {status}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        s.write_all(resp.as_bytes()).await?;
        s.shutdown().await
    }

    /// Carry out an action; also says which service's state changed.
    #[allow(clippy::type_complexity)]
    fn action(&self, action: &str, args: &HashMap<String, String>) -> (Result<Vec<(String, String)>, u32>, Option<&'static str>) {
        let mut st = self.state.lock().unwrap();
        st.calls.push((action.to_string(), args.clone()));
        let arg = |k: &str| args.get(k).cloned().unwrap_or_default();
        let ok = |v: Vec<(&str, String)>| Ok(v.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        match action {
            "GetTransportInfo" => (ok(vec![("CurrentTransportState", st.transport.clone()), ("CurrentTransportStatus", "OK".into()), ("CurrentSpeed", "1".into())]), None),
            "GetPositionInfo" => (
                ok(vec![("Track", "1".into()), ("TrackDuration", "0:03:00".into()), ("TrackMetaData", st.meta.clone()), ("TrackURI", st.uri.clone()), ("RelTime", st.position.clone()), ("AbsTime", st.position.clone())]),
                None,
            ),
            "GetMediaInfo" => (ok(vec![("NrTracks", "1".into()), ("MediaDuration", "0:03:00".into()), ("CurrentURI", st.uri.clone()), ("CurrentURIMetaData", st.meta.clone())]), None),
            "GetCurrentTransportActions" => (ok(vec![("Actions", Self::transport_actions(&st).into())]), None),
            "GetTransportSettings" => (ok(vec![("PlayMode", st.play_mode.clone()), ("RecQualityMode", "NOT_IMPLEMENTED".into())]), None),
            "GetVolume" => (ok(vec![("CurrentVolume", st.volume.to_string())]), None),
            "GetMute" => (ok(vec![("CurrentMute", u8::from(st.muted).to_string())]), None),
            "GetProtocolInfo" => (ok(vec![("Source", String::new()), ("Sink", SINK.into())]), None),
            "SetVolume" => {
                st.volume = arg("DesiredVolume").parse().unwrap_or(st.volume);
                (ok(vec![]), Some("RenderingControl"))
            }
            "SetMute" => {
                st.muted = arg("DesiredMute") == "1";
                (ok(vec![]), Some("RenderingControl"))
            }
            "SetAVTransportURI" => {
                st.uri = arg("CurrentURI");
                st.meta = arg("CurrentURIMetaData");
                st.transport = "STOPPED".into();
                (ok(vec![]), Some("AVTransport"))
            }
            "Play" if st.uri.is_empty() => (Err(701), None),
            "Play" => {
                st.transport = "PLAYING".into();
                (ok(vec![]), Some("AVTransport"))
            }
            "Pause" => {
                st.transport = "PAUSED_PLAYBACK".into();
                (ok(vec![]), Some("AVTransport"))
            }
            "Stop" => {
                st.transport = "STOPPED".into();
                (ok(vec![]), Some("AVTransport"))
            }
            "Seek" => {
                st.position = arg("Target");
                (ok(vec![]), None)
            }
            "SetPlayMode" => {
                st.play_mode = arg("NewPlayMode");
                (ok(vec![]), Some("AVTransport"))
            }
            "Next" | "Previous" => (ok(vec![]), None),
            _ => (Err(401), None),
        }
    }
}

async fn read_request(s: &mut TcpStream) -> std::io::Result<(String, String, HashMap<String, String>, String)> {
    let mut buf = vec![];
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_string();
    let path = first.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok((method, path, headers, String::from_utf8_lossy(&body).into_owned()))
}
