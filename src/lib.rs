//! DLNA: UPnP media renderers (TVs, receivers, speakers) as media players
//! in Homewright. An external integration.
//!
//! One setup covers every renderer on the network. Renderers are found with
//! SSDP while the setup runs and every minute after, plus any description
//! URLs the user typed for renderers SSDP cannot reach. The locations found
//! are remembered, so a renderer that is off when Homewright starts still
//! gets its media player. Each renderer pushes its state through GENA events
//! and is polled as well.

pub mod device;
pub mod events;
pub mod ssdp;
pub mod status;
pub mod upnp;
pub mod xml;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thehome_plugin::{async_trait, json, Device, Entity, Entry, Error, Flow, FlowReply, Host, Integration, Map, Value};

use device::{Ctx, DeviceHandle};
use upnp::Description;

pub const DOMAIN: &str = "dlna_dmr";
const SCAN: Duration = Duration::from_secs(4);
/// How often SSDP is asked again.
const RESEARCH: Duration = Duration::from_secs(60);
/// How often a URL the user typed is tried while the renderer is away.
const MANUAL_RETRY: Duration = Duration::from_secs(30);

/// Logging to Homewright's log through stderr.
#[doc(hidden)]
pub fn emit(level: &str, message: String) {
    thehome_plugin::log(level, message);
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::emit("INFO", format!($($t)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::emit("WARN", format!($($t)*)) };
}

/// A renderer as kept between flow steps and in the memory file.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub udn: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub location: String,
    pub tv: bool,
}

impl From<&Description> for Found {
    fn from(d: &Description) -> Self {
        Self { udn: d.udn.clone(), name: d.name.clone(), manufacturer: d.manufacturer.clone(), model: d.model.clone(), location: d.location.clone(), tv: d.tv }
    }
}

/// Where a renderer was found.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    Ssdp,
    /// A URL the user typed.
    Manual,
    /// The location SSDP gave last time.
    Remembered,
}

fn device_title(f: &Found) -> String {
    let host = reqwest::Url::parse(&f.location).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
    match &f.model {
        Some(m) if *m != f.name => format!("{} ({m}, {host})", f.name),
        _ => format!("{} ({host})", f.name),
    }
}

fn schema(found: &[Found], selected: &[String], urls: &str, listen_port: u64) -> Value {
    let mut props = Map::new();
    if !found.is_empty() {
        let options: Vec<Value> = found.iter().map(|f| json!({ "const": f.udn, "title": device_title(f) })).collect();
        props.insert(
            "devices".into(),
            json!({
                "type": "array", "title": "Afspillere", "items": { "type": "string", "oneOf": options }, "default": selected,
                "description": "Med alle valgt kommer nye afspillere også med, når de dukker op"
            }),
        );
    }
    props.insert(
        "urls".into(),
        json!({
            "type": "string", "title": "Ekstra adresser", "default": urls,
            "description": "URL'er til enhedsbeskrivelsen adskilt af komma, til afspillere som SSDP ikke finder, f.eks. http://192.168.20.15:9197/dmr"
        }),
    );
    props.insert(
        "listen_port".into(),
        json!({
            "type": "integer", "title": "Port til hændelser", "default": listen_port, "minimum": 0, "maximum": 65535,
            "description": "Den port, afspillerne sender deres ændringer til. 0 vælger en ledig port. Sæt en fast port, hvis den skal åbnes i en firewall"
        }),
    );
    json!({ "type": "object", "properties": props })
}

fn store(found: &[Found]) -> Value {
    found
        .iter()
        .map(|f| json!({ "udn": f.udn, "name": f.name, "manufacturer": f.manufacturer, "model": f.model, "location": f.location, "tv": f.tv }))
        .collect()
}

fn stored(data: &Map<String, Value>) -> Vec<Found> {
    let Some(list) = data.get("found").and_then(Value::as_array) else { return vec![] };
    list.iter()
        .filter_map(|f| {
            Some(Found {
                udn: f["udn"].as_str()?.into(),
                name: f["name"].as_str()?.into(),
                manufacturer: f["manufacturer"].as_str().map(String::from),
                model: f["model"].as_str().map(String::from),
                location: f["location"].as_str()?.into(),
                tv: f["tv"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

fn with_description(mut r: FlowReply, text: &str) -> FlowReply {
    r.result["description"] = json!(text);
    r
}

fn str_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
        Value::String(s) => s.split([',', ' ', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
        _ => vec![],
    }
}

fn port_of(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) if s.trim().is_empty() => Some(0),
        Value::String(s) => s.trim().parse().ok(),
        Value::Null => Some(0),
        _ => None,
    }
    .filter(|p| *p <= 65535)
}

/// Check the form and turn it into entry data, or show it again with errors.
#[allow(clippy::result_large_err)]
async fn submit(input: &Value, found_ids: &[String], extra: Map<String, Value>, show: impl Fn(Value) -> FlowReply) -> Result<Map<String, Value>, FlowReply> {
    let urls = str_list(&input["urls"]);
    let client = upnp::client();
    for u in &urls {
        if !reqwest::Url::parse(u).is_ok_and(|p| matches!(p.scheme(), "http" | "https") && p.host_str().is_some()) {
            info!("ugyldig adresse: {u}");
            return Err(show(json!({ "urls": "invalid_url" })));
        }
        match upnp::fetch_description(&client, u).await {
            Ok(d) if upnp::is_renderer(&d) => {}
            Ok(d) => {
                info!("{u}: {} er ikke en afspiller", d.name);
                return Err(show(json!({ "urls": "not_renderer" })));
            }
            Err(e) => {
                info!("{e}");
                return Err(show(json!({ "base": "cannot_connect" })));
            }
        }
    }
    let Some(port) = port_of(&input["listen_port"]) else {
        return Err(show(json!({ "listen_port": "invalid_port" })));
    };
    let mut data = extra;
    data.insert("urls".into(), json!(urls));
    if port > 0 {
        data.insert("listen_port".into(), json!(port));
    }
    let chosen: Vec<String> = str_list(&input["devices"]).into_iter().filter(|d| found_ids.contains(d)).collect();
    // all of them means new renderers too, so nothing is stored
    if !found_ids.is_empty() && !found_ids.iter().all(|d| chosen.contains(d)) {
        data.insert("devices".into(), json!(chosen));
    }
    Ok(data)
}

/// Renderers SSDP finds now, with their descriptions. Sonos players have
/// their own integration and are left out.
async fn scan() -> Vec<Found> {
    let client = upnp::client();
    let mut tasks = tokio::task::JoinSet::new();
    for s in ssdp::search(SCAN).await {
        let client = client.clone();
        tasks.spawn(async move { upnp::fetch_description(&client, &s.location).await.ok() });
    }
    let mut out = vec![];
    while let Some(r) = tasks.join_next().await {
        if let Ok(Some(d)) = r {
            if upnp::is_renderer(&d) && !d.is_sonos() && !out.iter().any(|f: &Found| f.udn == d.udn) {
                out.push(Found::from(&d));
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Default)]
pub struct Dlna {
    entries: Mutex<HashMap<String, Arc<Runtime>>>,
}

impl Dlna {
    fn runtime(&self, entry_id: &str) -> Result<Arc<Runtime>, Error> {
        self.entries.lock().unwrap().get(entry_id).cloned().ok_or_else(|| Error::unavailable("opsætningen kører ikke"))
    }

    /// Renderers to offer in a form: a fresh scan plus what the setup knows.
    async fn found(&self, flow: &Flow) -> Vec<Found> {
        let mut found = if flow.data.get("scan") == Some(&json!(false)) { vec![] } else { scan().await };
        let known = flow.entry.as_ref().and_then(|e| self.entries.lock().unwrap().get(&e.entry_id).cloned());
        if let Some(rt) = known {
            for f in rt.ssdp_devices().await {
                if !found.iter().any(|x| x.udn == f.udn) {
                    found.push(f);
                }
            }
        }
        for f in stored(&flow.data) {
            if !found.iter().any(|x| x.udn == f.udn) {
                found.push(f);
            }
        }
        found
    }
}

#[async_trait]
impl Integration for Dlna {
    async fn config_flow(&self, _host: &Host, flow: Flow, step: &str, input: Option<Value>) -> Result<FlowReply, Error> {
        match (step, input) {
            ("ssdp", None) => {
                let d = flow.discovery.clone().unwrap_or(Value::Null);
                let location = d["location"].as_str().unwrap_or_default().to_string();
                let desc = upnp::fetch_description(&upnp::client(), &location).await.ok();
                if desc.as_ref().is_some_and(Description::is_sonos) {
                    return Ok(FlowReply::abort("not_supported"));
                }
                let name = desc.map(|d| d.name).or(d["host"].as_str().map(String::from)).unwrap_or_else(|| "en DLNA-afspiller".into());
                let r = FlowReply::form("confirm", json!({ "type": "object", "properties": {} })).with_unique_id(DOMAIN);
                Ok(with_description(r, &format!("Fandt {name}. Alle DLNA-afspillere på netværket bliver tilføjet, også dem der dukker op senere.")))
            }
            ("confirm", Some(_)) => Ok(FlowReply::create("DLNA", json!({})).with_unique_id(DOMAIN)),
            ("user", None) => {
                let found = self.found(&flow).await;
                let ids: Vec<String> = found.iter().map(|f| f.udn.clone()).collect();
                let mut data = flow.data.clone();
                data.insert("found".into(), store(&found));
                let r = FlowReply::form("user", schema(&found, &ids, "", 0)).with_data(data).with_unique_id(DOMAIN);
                Ok(if found.is_empty() {
                    with_description(r, "Ingen DLNA-afspillere fundet på netværket lige nu. Afspillere, der dukker op senere, bliver tilføjet.")
                } else {
                    r
                })
            }
            ("user", Some(v)) => {
                let found = stored(&flow.data);
                let ids: Vec<String> = found.iter().map(|f| f.udn.clone()).collect();
                let mut extra = Map::new();
                if flow.data.get("ssdp") == Some(&json!(false)) {
                    extra.insert("ssdp".into(), json!(false));
                }
                let typed = v["urls"].as_str().unwrap_or_default().to_string();
                let port = port_of(&v["listen_port"]).unwrap_or(0);
                let chosen = str_list(&v["devices"]);
                let show = |errors| FlowReply::form_with_errors("user", schema(&found, &chosen, &typed, port), errors);
                match submit(&v, &ids, extra, show).await {
                    Ok(data) => Ok(FlowReply::create("DLNA", Value::Object(data)).with_unique_id(DOMAIN)),
                    Err(form) => Ok(form),
                }
            }
            _ => Ok(FlowReply::abort("unknown_step")),
        }
    }

    async fn options_flow(&self, _host: &Host, flow: Flow, _step: &str, input: Option<Value>) -> Result<FlowReply, Error> {
        let Some(entry) = flow.entry.clone() else { return Ok(FlowReply::abort("unknown")) };
        let urls = str_list(entry.get("urls").unwrap_or(&Value::Null)).join(", ");
        let port = entry.get("listen_port").and_then(Value::as_u64).unwrap_or(0);
        match input {
            None => {
                let found = self.found(&flow).await;
                let ids: Vec<String> = found.iter().map(|f| f.udn.clone()).collect();
                let current = entry.get("devices").map(str_list).unwrap_or_else(|| ids.clone());
                let mut data = flow.data.clone();
                data.insert("found".into(), store(&found));
                Ok(FlowReply::form("init", schema(&found, &current, &urls, port)).with_data(data))
            }
            Some(v) => {
                let found = stored(&flow.data);
                let ids: Vec<String> = found.iter().map(|f| f.udn.clone()).collect();
                let typed = v["urls"].as_str().unwrap_or_default().to_string();
                let typed_port = port_of(&v["listen_port"]).unwrap_or(0);
                let chosen = str_list(&v["devices"]);
                let show = |errors| FlowReply::form_with_errors("init", schema(&found, &chosen, &typed, typed_port), errors);
                match submit(&v, &ids, Map::new(), show).await {
                    Ok(data) => Ok(FlowReply::create("", Value::Object(data))),
                    Err(form) => Ok(form),
                }
            }
        }
    }

    async fn setup_entry(&self, host: &Host, entry: Entry) -> Result<(), Error> {
        let rt = Runtime::start(host.clone(), &entry).await;
        if let Some(old) = self.entries.lock().unwrap().insert(entry.entry_id.clone(), rt) {
            old.stop();
        }
        Ok(())
    }

    async fn unload_entry(&self, _host: &Host, entry_id: &str) -> Result<(), Error> {
        if let Some(rt) = self.entries.lock().unwrap().remove(entry_id) {
            rt.stop();
        }
        Ok(())
    }

    async fn entity_service(&self, _host: &Host, entry_id: &str, entity_id: &str, service: &str, data: Map<String, Value>) -> Result<(), Error> {
        let rt = self.runtime(entry_id)?;
        rt.call(entity_id, service, data).await
    }

    async fn diagnostics(&self, _host: &Host, entry: Entry) -> Value {
        let mut d = json!({ "data": entry.data, "options": entry.options });
        if let Ok(rt) = self.runtime(&entry.entry_id) {
            d["event_port"] = json!(rt.ctx.events.as_ref().map(|e| e.port()));
            d["devices"] = Value::Array(rt.diagnostics().await);
        }
        d
    }
}

struct Known {
    info: Found,
    manual: bool,
    entity_id: Option<String>,
    handle: Option<DeviceHandle>,
}

enum Msg {
    Seen(ssdp::Sighting),
    Found(Found, Source),
}

/// One running setup.
struct Runtime {
    host: Host,
    entry_id: String,
    ctx: Arc<Ctx>,
    /// Chosen renderers; `None` is all of them.
    selection: Option<Vec<String>>,
    devices: tokio::sync::Mutex<HashMap<String, Known>>,
    /// Seen with SSDP but not wanted: Sonos, not chosen, not a renderer.
    ignored: Mutex<HashSet<String>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Runtime {
    async fn start(host: Host, entry: &Entry) -> Arc<Self> {
        let selection = entry.get("devices").map(str_list);
        let urls = str_list(entry.get("urls").unwrap_or(&Value::Null));
        let listen_port = entry.get("listen_port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
        // for tests: a shorter poll interval in seconds
        let poll = entry.get_f64("poll_interval").filter(|p| *p > 0.0).map(Duration::from_secs_f64);
        let events = match events::EventServer::start(listen_port).await {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                warn!("kan ikke lytte efter hændelser på port {listen_port}: {e}; afspillerne bliver kun spurgt");
                None
            }
        };
        let fast = poll.unwrap_or(Duration::from_secs(5));
        let ctx = Arc::new(Ctx { client: upnp::client(), events, fast, slow: fast * 6, retry: poll.map(|p| p * 6).unwrap_or(Duration::from_secs(30)) });
        let rt = Arc::new(Self {
            host,
            entry_id: entry.entry_id.clone(),
            ctx,
            selection,
            devices: Default::default(),
            ignored: Default::default(),
            tasks: Default::default(),
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
        for f in remembered(&rt.memory_file()) {
            let _ = tx.send(Msg::Found(f, Source::Remembered));
        }
        if entry.get("ssdp") != Some(&json!(false)) {
            let tx = tx.clone();
            rt.tasks.lock().unwrap().push(tokio::spawn(async move {
                loop {
                    for s in ssdp::search(SCAN).await {
                        if tx.send(Msg::Seen(s)).is_err() {
                            return;
                        }
                    }
                    tokio::time::sleep(RESEARCH).await;
                }
            }));
        }
        for url in urls {
            let tx = tx.clone();
            let client = rt.ctx.client.clone();
            let retry = poll.map(|p| p * 6).unwrap_or(MANUAL_RETRY);
            rt.tasks.lock().unwrap().push(tokio::spawn(async move {
                loop {
                    match upnp::fetch_description(&client, &url).await {
                        Ok(d) if upnp::is_renderer(&d) => {
                            let _ = tx.send(Msg::Found(Found::from(&d), Source::Manual));
                            break;
                        }
                        Ok(d) => {
                            warn!("{url}: {} er ikke en afspiller", d.name);
                            break;
                        }
                        Err(e) => info!("{e}"),
                    }
                    tokio::time::sleep(retry).await;
                }
            }));
        }
        drop(tx);
        let me = rt.clone();
        rt.tasks.lock().unwrap().push(tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                match m {
                    Msg::Seen(s) => me.on_seen(s).await,
                    Msg::Found(f, source) => me.on_found(f, source).await,
                }
            }
        }));
        rt
    }

    fn stop(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        // the handles stop their tasks when dropped
        if let Ok(mut d) = self.devices.try_lock() {
            d.clear();
        }
    }

    fn memory_file(&self) -> std::path::PathBuf {
        self.host.data_dir().join(format!("devices-{}.json", self.entry_id))
    }

    /// Write down where the renderers SSDP found are.
    fn remember(&self, devices: &HashMap<String, Known>) {
        let mut found: Vec<Found> = devices.values().filter(|k| !k.manual).map(|k| k.info.clone()).collect();
        found.sort_by(|a, b| a.udn.cmp(&b.udn));
        let path = self.memory_file();
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&store(&found))?)
        };
        if let Err(e) = write() {
            warn!("kan ikke gemme afspillernes adresser i {}: {e}", path.display());
        }
    }

    async fn on_seen(&self, s: ssdp::Sighting) {
        {
            let mut devices = self.devices.lock().await;
            if let Some(k) = devices.get_mut(&s.udn) {
                if let Some(h) = &k.handle {
                    if !k.manual || k.info.location == s.location {
                        h.seen(&s.location);
                    }
                }
                if !k.manual && k.info.location != s.location {
                    k.info.location = s.location.clone();
                    self.remember(&devices);
                }
                return;
            }
        }
        if self.ignored.lock().unwrap().contains(&s.udn) {
            return;
        }
        if self.selection.as_ref().is_some_and(|sel| !sel.contains(&s.udn)) {
            self.ignored.lock().unwrap().insert(s.udn);
            return;
        }
        match upnp::fetch_description(&self.ctx.client, &s.location).await {
            Ok(d) if !upnp::is_renderer(&d) || (d.is_sonos() && self.selection.is_none()) => {
                self.ignored.lock().unwrap().insert(s.udn);
            }
            Ok(d) => self.on_found(Found::from(&d), Source::Ssdp).await,
            // tried again at the next search
            Err(e) => info!("{e}"),
        }
    }

    async fn on_found(&self, f: Found, source: Source) {
        let manual = source == Source::Manual;
        if !manual && self.selection.as_ref().is_some_and(|s| !s.contains(&f.udn)) {
            return;
        }
        let mut devices = self.devices.lock().await;
        if let Some(k) = devices.get_mut(&f.udn) {
            if source == Source::Remembered || (k.manual && !manual) {
                return;
            }
            k.manual |= manual;
            if k.info.location != f.location {
                k.info.location = f.location.clone();
                if let Some(h) = &k.handle {
                    h.seen(&f.location);
                }
                self.remember(&devices);
            }
            return;
        }
        let how = if source == Source::Remembered { "husker" } else { "fandt" };
        info!("{how} {} ({}) på {}", f.name, f.model.as_deref().unwrap_or("ukendt model"), f.location);
        let dev = Device {
            identifiers: vec![(DOMAIN.into(), f.udn.clone())],
            name: f.name.clone(),
            manufacturer: f.manufacturer.clone(),
            model: f.model.clone(),
            ..Default::default()
        };
        let mut known = Known { info: f.clone(), manual, entity_id: None, handle: None };
        match self.host.register_device(&self.entry_id, dev).await {
            Ok(dev_id) => {
                let mut e = Entity::new("media_player", f.udn.clone()).device(&dev_id);
                if f.tv {
                    e = e.device_class("tv");
                }
                match self.host.add_entity(&self.entry_id, e, true).await {
                    Ok(Some(entity_id)) => {
                        known.handle = Some(DeviceHandle::spawn(self.host.clone(), entity_id.clone(), f.udn.clone(), f.name.clone(), f.location.clone(), self.ctx.clone()));
                        known.entity_id = Some(entity_id);
                    }
                    Ok(None) => {}
                    Err(e) => warn!("{}: {e}", f.name),
                }
            }
            Err(e) => warn!("{}: {e}", f.name),
        }
        devices.insert(f.udn.clone(), known);
        if source == Source::Ssdp {
            self.remember(&devices);
        }
    }

    async fn call(&self, entity_id: &str, service: &str, data: Map<String, Value>) -> Result<(), Error> {
        let devices = self.devices.lock().await;
        let k = devices
            .values()
            .find(|k| k.entity_id.as_deref() == Some(entity_id))
            .ok_or_else(|| Error::new("not_found", format!("{entity_id} findes ikke")))?;
        let caller = k.handle.as_ref().ok_or_else(|| Error::unavailable(format!("{entity_id} er slået fra")))?.caller();
        // don't hold the lock while the renderer answers
        drop(devices);
        caller.call(service, data).await
    }

    async fn ssdp_devices(&self) -> Vec<Found> {
        self.devices.lock().await.values().filter(|k| !k.manual).map(|k| k.info.clone()).collect()
    }

    async fn diagnostics(&self) -> Vec<Value> {
        let devices: Vec<(Found, bool, Option<String>, Option<device::Caller>)> = self
            .devices
            .lock()
            .await
            .values()
            .map(|k| (k.info.clone(), k.manual, k.entity_id.clone(), k.handle.as_ref().map(DeviceHandle::caller)))
            .collect();
        let mut out = vec![];
        for (info, manual, entity_id, caller) in devices {
            let status = match caller {
                Some(c) => c.snapshot().await,
                None => Value::Null,
            };
            out.push(json!({
                "udn": info.udn, "name": info.name, "model": info.model, "manufacturer": info.manufacturer,
                "location": info.location, "manual": manual, "entity_id": entity_id, "status": status,
            }));
        }
        out
    }
}

/// The renderers written down last time.
fn remembered(path: &std::path::Path) -> Vec<Found> {
    let Ok(bytes) = std::fs::read(path) else { return vec![] };
    let list: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    stored(&Map::from_iter([("found".to_string(), list)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_values() {
        assert_eq!(str_list(&json!("http://a/x, http://b/y\nhttp://c/z")), ["http://a/x", "http://b/y", "http://c/z"]);
        assert_eq!(port_of(&json!("8099")), Some(8099));
        assert_eq!(port_of(&json!("")), Some(0));
        assert_eq!(port_of(&json!(70000)), None);
        let found = vec![Found { udn: "u".into(), name: "TV".into(), model: Some("UE50".into()), manufacturer: None, location: "http://10.0.0.9:9197/dmr".into(), tv: true }];
        assert_eq!(stored(&Map::from_iter([("found".to_string(), store(&found))])), found);
        assert_eq!(device_title(&found[0]), "TV (UE50, 10.0.0.9)");
    }
}
