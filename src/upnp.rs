//! UPnP over HTTP: the device description, the service descriptions (SCPD),
//! SOAP actions and GENA subscriptions.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use reqwest::{Client, Method, Url};

use crate::xml::{self, Node};

pub fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(3))
        .user_agent("HomewrightHQ/1 UPnP/1.0 DLNADOC/1.50")
        .build()
        .expect("http client")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    /// `AVTransport`, `RenderingControl`, `ConnectionManager` ...
    pub kind: String,
    pub service_type: String,
    pub control: String,
    pub event: String,
    pub scpd: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Description {
    pub location: String,
    /// The UDN without `uuid:`.
    pub udn: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    /// Whether the device says it is a TV.
    pub tv: bool,
    pub services: Vec<Service>,
}

impl Description {
    pub fn service(&self, kind: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.kind == kind)
    }

    /// The host part of the location.
    pub fn host(&self) -> String {
        Url::parse(&self.location).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default()
    }

    pub fn is_sonos(&self) -> bool {
        self.manufacturer.as_deref().is_some_and(|m| m.to_lowercase().contains("sonos"))
    }
}

/// `rel` against `base`; an absolute `rel` is kept.
pub fn absolute(base: &str, rel: &str) -> String {
    match Url::parse(base).and_then(|b| b.join(rel.trim())) {
        Ok(u) => u.to_string(),
        Err(_) => rel.to_string(),
    }
}

fn renderer_device(root: &Node) -> Option<&Node> {
    fn walk(d: &Node) -> Option<&Node> {
        if d.text_of("deviceType").is_some_and(|t| t.contains(":MediaRenderer:")) {
            return Some(d);
        }
        d.child("deviceList")?.children_named("device").find_map(walk)
    }
    let top = root.child("device")?;
    walk(top).or(Some(top))
}

/// Parse a device description fetched from `location`.
pub fn parse_description(location: &str, text: &str) -> Result<Description, String> {
    let root = xml::parse(text).ok_or("enhedsbeskrivelsen er ikke gyldig XML")?;
    let base = root.text_of("URLBase").unwrap_or(location).to_string();
    let top = root.child("device").ok_or("enhedsbeskrivelsen har ingen <device>")?;
    let d = renderer_device(&root).unwrap_or(top);
    let udn = d.text_of("UDN").or(top.text_of("UDN")).ok_or("enheden har ingen UDN")?;
    let udn = udn.trim_start_matches("uuid:").to_lowercase();
    let mut services = vec![];
    if let Some(list) = d.child("serviceList") {
        for s in list.children_named("service") {
            let Some(st) = s.text_of("serviceType") else { continue };
            let kind = st.split(':').nth(3).unwrap_or(st).to_string();
            let url = |k: &str| s.text_of(k).map(|u| absolute(&base, u)).unwrap_or_default();
            services.push(Service { kind, service_type: st.to_string(), control: url("controlURL"), event: url("eventSubURL"), scpd: url("SCPDURL") });
        }
    }
    let text = |n: &str| d.text_of(n).map(String::from);
    let name = text("friendlyName").unwrap_or_else(|| Url::parse(location).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default());
    let category = d.text_of("X_deviceCategory").unwrap_or_default();
    let tv = category.contains("Display") || d.text_of("modelDescription").is_some_and(|m| m.contains("TV")) || name.contains(" TV");
    Ok(Description {
        location: location.to_string(),
        udn,
        name,
        manufacturer: text("manufacturer"),
        model: text("modelName"),
        serial: text("serialNumber"),
        tv,
        services,
    })
}

pub async fn fetch_description(client: &Client, location: &str) -> Result<Description, String> {
    let r = client.get(location).send().await.map_err(|e| format!("{location}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{location}: HTTP {}", r.status()));
    }
    let body = r.text().await.map_err(|e| e.to_string())?;
    parse_description(location, &body)
}

/// Whether a description is a renderer that can be controlled.
pub fn is_renderer(d: &Description) -> bool {
    d.service("AVTransport").is_some() || d.service("RenderingControl").is_some()
}

/// What a service description says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scpd {
    pub actions: HashSet<String>,
    /// state variable -> allowed values
    pub allowed: HashMap<String, Vec<String>>,
    /// state variable -> maximum
    pub maximum: HashMap<String, f64>,
}

pub fn parse_scpd(text: &str) -> Scpd {
    let mut s = Scpd::default();
    let Some(root) = xml::parse(text) else { return s };
    if let Some(list) = root.child("actionList") {
        for a in list.children_named("action") {
            if let Some(n) = a.text_of("name") {
                s.actions.insert(n.to_string());
            }
        }
    }
    if let Some(table) = root.child("serviceStateTable") {
        for v in table.children_named("stateVariable") {
            let Some(name) = v.text_of("name") else { continue };
            if let Some(list) = v.child("allowedValueList") {
                s.allowed.insert(name.into(), list.children_named("allowedValue").map(|a| a.text.trim().to_string()).collect());
            }
            if let Some(max) = v.child("allowedValueRange").and_then(|r| r.text_of("maximum")).and_then(|m| m.parse().ok()) {
                s.maximum.insert(name.into(), max);
            }
        }
    }
    s
}

pub async fn fetch_scpd(client: &Client, url: &str) -> Option<Scpd> {
    let r = client.get(url).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    let s = parse_scpd(&r.text().await.ok()?);
    (!s.actions.is_empty()).then_some(s)
}

#[derive(Debug, Clone, PartialEq)]
pub enum SoapError {
    /// The device did not answer.
    Unreachable(String),
    /// A UPnP error code and its description.
    Upnp(u32, String),
    Other(String),
}

impl std::fmt::Display for SoapError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "enheden svarer ikke: {e}"),
            Self::Upnp(code, desc) if desc.is_empty() => write!(f, "UPnP-fejl {code}"),
            Self::Upnp(code, desc) => write!(f, "UPnP-fejl {code}: {desc}"),
            Self::Other(e) => f.write_str(e),
        }
    }
}

pub fn envelope(service_type: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut body = String::new();
    for (k, v) in args {
        body.push_str(&format!("<{k}>{}</{k}>", xml::escape(v)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_type}\">{body}</u:{action}></s:Body></s:Envelope>"
    )
}

/// Call `action` and return the output arguments.
pub async fn call(client: &Client, svc: &Service, action: &str, args: &[(&str, String)]) -> Result<HashMap<String, String>, SoapError> {
    let r = client
        .post(&svc.control)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", format!("\"{}#{action}\"", svc.service_type))
        .body(envelope(&svc.service_type, action, args))
        .send()
        .await
        .map_err(|e| SoapError::Unreachable(e.to_string()))?;
    let status = r.status();
    let text = r.text().await.map_err(|e| SoapError::Unreachable(e.to_string()))?;
    let doc = xml::parse(&text);
    if !status.is_success() {
        let err = doc.as_ref().and_then(|d| d.find("UPnPError"));
        return Err(match err {
            Some(e) => SoapError::Upnp(e.text_of("errorCode").and_then(|c| c.parse().ok()).unwrap_or(0), e.text_of("errorDescription").unwrap_or_default().to_string()),
            None => SoapError::Other(format!("{action}: HTTP {status}")),
        });
    }
    let body = doc.as_ref().and_then(|d| d.find("Body")).ok_or_else(|| SoapError::Other(format!("{action}: svaret er ikke SOAP")))?;
    let out = body.children.first().map(|resp| resp.children.iter().map(|c| (c.name.clone(), c.text.clone())).collect()).unwrap_or_default();
    Ok(out)
}

/// Subscribe to a service's events. Returns the SID and the timeout in seconds.
pub async fn subscribe(client: &Client, url: &str, callback: &str, timeout: u64) -> Result<(String, u64), String> {
    let r = client
        .request(Method::from_bytes(b"SUBSCRIBE").unwrap(), url)
        .header("CALLBACK", format!("<{callback}>"))
        .header("NT", "upnp:event")
        .header("TIMEOUT", format!("Second-{timeout}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    sid_and_timeout(r, timeout)
}

pub async fn renew(client: &Client, url: &str, sid: &str, timeout: u64) -> Result<(String, u64), String> {
    let r = client
        .request(Method::from_bytes(b"SUBSCRIBE").unwrap(), url)
        .header("SID", sid)
        .header("TIMEOUT", format!("Second-{timeout}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    sid_and_timeout(r, timeout)
}

pub async fn unsubscribe(client: &Client, url: &str, sid: &str) {
    let _ = client.request(Method::from_bytes(b"UNSUBSCRIBE").unwrap(), url).header("SID", sid).send().await;
}

fn sid_and_timeout(r: reqwest::Response, asked: u64) -> Result<(String, u64), String> {
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status()));
    }
    let h = |k: &str| r.headers().get(k).and_then(|v| v.to_str().ok()).map(str::trim).unwrap_or_default().to_string();
    let sid = h("SID");
    if sid.is_empty() {
        return Err("svaret har intet SID".into());
    }
    let timeout = h("TIMEOUT").to_lowercase().strip_prefix("second-").and_then(|t| t.parse().ok()).unwrap_or(asked);
    Ok((sid, timeout.max(30)))
}

/// The variables in a GENA event: name and text of each property.
pub fn properties(body: &str) -> Vec<(String, String)> {
    let Some(root) = xml::parse(body) else { return vec![] };
    root.children_named("property").flat_map(|p| p.children.iter().map(|c| (c.name.clone(), c.text.clone()))).collect()
}

/// A `LastChange` value: each variable of instance 0 with its `val`, and
/// the channel when there is one (`Volume channel="Master"`).
pub fn last_change(text: &str) -> Vec<(String, Option<String>, String)> {
    let Some(root) = xml::parse(text) else { return vec![] };
    let mut out = vec![];
    for inst in root.children_named("InstanceID") {
        if inst.attr("val").is_some_and(|v| v != "0") {
            continue;
        }
        for v in &inst.children {
            out.push((v.name.clone(), v.attr("channel").map(String::from), v.attr("val").unwrap_or_default().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SONOS: &str = r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><device>
        <deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType><friendlyName>192.168.0.9 - Sonos One</friendlyName>
        <manufacturer>Sonos, Inc.</manufacturer><UDN>uuid:RINCON_1</UDN>
        <deviceList><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
          <friendlyName>Stue - Sonos One Media Renderer</friendlyName><manufacturer>Sonos, Inc.</manufacturer><UDN>uuid:RINCON_1_MR</UDN>
          <serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
            <controlURL>/MediaRenderer/AVTransport/Control</controlURL><eventSubURL>/MediaRenderer/AVTransport/Event</eventSubURL><SCPDURL>/xml/AVTransport1.xml</SCPDURL></service></serviceList>
        </device></deviceList></device></root>"#;

    #[test]
    fn embedded_renderer() {
        let d = parse_description("http://192.168.0.9:1400/xml/device_description.xml", SONOS).unwrap();
        assert_eq!(d.udn, "rincon_1_mr");
        assert_eq!(d.name, "Stue - Sonos One Media Renderer");
        assert!(d.is_sonos());
        assert_eq!(d.service("AVTransport").unwrap().control, "http://192.168.0.9:1400/MediaRenderer/AVTransport/Control");
        assert_eq!(d.host(), "192.168.0.9");
    }

    #[test]
    fn last_change_values() {
        let lc = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"><InstanceID val="0"><Volume channel="Master" val="12"/><Mute channel="Master" val="0"/></InstanceID></Event>"#;
        let body = format!(r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#, xml::escape(lc));
        let props = properties(&body);
        assert_eq!(props[0].0, "LastChange");
        let vars = last_change(&props[0].1);
        assert_eq!(vars[0], ("Volume".into(), Some("Master".into()), "12".into()));
    }

    #[test]
    fn soap_envelope_escapes() {
        let e = envelope("urn:schemas-upnp-org:service:AVTransport:1", "SetAVTransportURI", &[("CurrentURI", "http://x/a?b=1&c=2".into())]);
        assert!(e.contains("<CurrentURI>http://x/a?b=1&amp;c=2</CurrentURI>"));
        assert!(e.contains("<u:SetAVTransportURI xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">"));
    }
}
