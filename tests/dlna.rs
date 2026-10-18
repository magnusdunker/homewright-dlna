//! The plugin driven over a pipe the way Homewright drives it, against a
//! fake renderer on localhost.

mod fake;
mod host;

use std::sync::atomic::Ordering;
use std::time::Duration;

use fake::FakeRenderer;
use homewright_dlna::Dlna;
use host::FakeHost;
use serde_json::{json, Value};

async fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..300 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ventede forgæves på {what}");
}

fn flow(source: &str, data: Value) -> Value {
    json!({ "flow_id": "f1", "source": source, "data": data })
}

async fn start(name: &str) -> FakeHost {
    let host = FakeHost::start(Dlna::default());
    let dir = std::env::temp_dir().join(format!("dlna-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    host.call("initialize", json!({ "api_version": 1, "data_dir": dir })).await.unwrap();
    host
}

fn entry(r: &FakeRenderer) -> Value {
    json!({ "entry_id": "e1", "domain": "dlna_dmr", "data": { "ssdp": false, "urls": [r.location()], "poll_interval": 0.5 }, "options": {} })
}

async fn service(host: &FakeHost, entity: &str, service: &str, data: Value) -> Result<Value, Value> {
    host.call("entity/call_service", json!({ "entry_id": "e1", "entity_id": entity, "service": service, "data": data })).await
}

const UID: &str = "fake-renderer-1";

fn attr(host: &FakeHost, key: &str) -> Value {
    host.state_of(UID).map(|(_, a)| a[key].clone()).unwrap_or(Value::Null)
}

fn state(host: &FakeHost) -> String {
    host.state_of(UID).map(|(s, _)| s).unwrap_or_default()
}

#[tokio::test]
async fn user_flow_checks_urls() {
    let renderer = FakeRenderer::start(true).await;
    let server = FakeRenderer::start_with("fake-server", true, false).await;
    let host = start("flow").await;
    let data = json!({ "scan": false, "ssdp": false,
                       "found": [{ "udn": "aa", "name": "Stue TV", "location": "http://10.0.0.20:9197/dmr", "model": "UE50", "tv": true },
                                 { "udn": "bb", "name": "Køkken", "location": "http://10.0.0.21:49152/desc.xml" }] });
    let r = host.call("config_flow", json!({ "flow": flow("user", data), "step": "user" })).await.unwrap();
    assert_eq!(r["result"]["type"], "form");
    assert_eq!(r["unique_id"], "dlna_dmr");
    let props: Vec<&String> = r["result"]["schema"]["properties"].as_object().unwrap().keys().collect();
    assert_eq!(props, ["devices", "urls", "listen_port"]);
    assert_eq!(r["result"]["schema"]["properties"]["devices"]["items"]["oneOf"][0]["title"], "Stue TV (UE50, 10.0.0.20)");
    let data = r["data"].clone();

    let step = |input: Value| json!({ "flow": flow("user", data.clone()), "step": "user", "input": input });
    let r = host.call("config_flow", step(json!({ "urls": "ikke en url" }))).await.unwrap();
    assert_eq!(r["result"]["errors"]["urls"], "invalid_url");
    let r = host.call("config_flow", step(json!({ "urls": "http://127.0.0.1:1/dmr" }))).await.unwrap();
    assert_eq!(r["result"]["errors"]["base"], "cannot_connect");
    let r = host.call("config_flow", step(json!({ "urls": server.location() }))).await.unwrap();
    assert_eq!(r["result"]["errors"]["urls"], "not_renderer");
    let r = host.call("config_flow", step(json!({ "urls": "", "listen_port": "abc" }))).await.unwrap();
    assert_eq!(r["result"]["errors"]["listen_port"], "invalid_port");

    let r = host.call("config_flow", step(json!({ "urls": renderer.location(), "devices": ["aa", "bb"], "listen_port": 0 }))).await.unwrap();
    assert_eq!(r["result"]["type"], "create_entry");
    assert_eq!(r["result"]["title"], "DLNA");
    assert_eq!(r["result"]["data"], json!({ "ssdp": false, "urls": [renderer.location()] }));
    let r = host.call("config_flow", step(json!({ "urls": "", "devices": ["bb"], "listen_port": 8099 }))).await.unwrap();
    assert_eq!(r["result"]["data"], json!({ "ssdp": false, "urls": [], "listen_port": 8099, "devices": ["bb"] }));
}

#[tokio::test]
async fn ssdp_flow_asks_once() {
    let renderer = FakeRenderer::start(true).await;
    let host = start("ssdp").await;
    let disc = json!({ "location": renderer.location(), "host": "127.0.0.1", "st": "urn:schemas-upnp-org:device:MediaRenderer:1" });
    let f = json!({ "flow_id": "f2", "source": "ssdp", "discovery": disc, "data": {} });
    let r = host.call("config_flow", json!({ "flow": f, "step": "ssdp" })).await.unwrap();
    assert_eq!(r["result"]["step_id"], "confirm");
    assert_eq!(r["unique_id"], "dlna_dmr");
    assert!(r["result"]["description"].as_str().unwrap().contains("Stue TV"));
    let r = host.call("config_flow", json!({ "flow": f, "step": "confirm", "input": {} })).await.unwrap();
    assert_eq!(r["result"]["type"], "create_entry");
    assert_eq!(r["result"]["data"], json!({}));
}

#[tokio::test]
async fn controls_a_renderer() {
    let r = FakeRenderer::start(true).await;
    let host = start("control").await;
    host.call("setup_entry", json!({ "entry": entry(&r) })).await.unwrap();

    let (s, attrs) = wait_for("tilstand", || host.state_of(UID)).await;
    assert_eq!(s, "idle");
    assert_eq!(attrs["volume_level"], 0.1);
    assert_eq!(attrs["is_volume_muted"], false);
    let id = host.entity(UID);
    {
        let rec = host.rec.lock().unwrap();
        assert_eq!(rec.devices[0]["name"], "Stue TV");
        assert_eq!(rec.devices[0]["manufacturer"], "Fake Electronics");
        let (spec, handler) = &rec.entities[&id];
        assert_eq!(spec["device_class"], "tv");
        assert!(handler);
    }
    // nothing loaded: no play or pause, but play_media and volume
    let f = attrs["supported_features"].as_u64().unwrap();
    assert_eq!(f & (16384 | 1), 0);
    assert_ne!(f & 512, 0);
    assert_ne!(f & 4, 0);

    service(&host, &id, "volume_set", json!({ "volume_level": 0.4 })).await.unwrap();
    assert_eq!(r.state.lock().unwrap().volume, 40);
    wait_for("lydstyrke", || (attr(&host, "volume_level") == 0.4).then_some(())).await;
    service(&host, &id, "volume_up", json!({})).await.unwrap();
    assert_eq!(r.state.lock().unwrap().volume, 45);
    service(&host, &id, "volume_mute", json!({ "is_volume_muted": true })).await.unwrap();
    wait_for("lyd slået fra", || (attr(&host, "is_volume_muted") == true).then_some(())).await;

    service(&host, &id, "play_media", json!({ "media_content_id": "http://nas.lan/musik/sang.mp3", "media_content_type": "music", "extra": { "title": "Sang & co" } }))
        .await
        .unwrap();
    let set = r.calls("SetAVTransportURI");
    assert_eq!(set[0]["CurrentURI"], "http://nas.lan/musik/sang.mp3");
    let meta = &set[0]["CurrentURIMetaData"];
    assert!(meta.contains("protocolInfo=\"http-get:*:audio/mpeg:DLNA.ORG_FLAGS=ED100000000000000000000000000000\""), "{meta}");
    assert!(meta.contains("object.item.audioItem.musicTrack"));
    assert_eq!(r.calls("Play").len(), 1);
    wait_for("afspiller", || (state(&host) == "playing").then_some(())).await;
    assert_eq!(attr(&host, "media_title"), "Sang & co");
    assert_eq!(attr(&host, "media_content_id"), "http://nas.lan/musik/sang.mp3");
    assert_eq!(attr(&host, "media_content_type"), "music");
    assert_eq!(attr(&host, "media_duration"), 180.0);
    assert_ne!(attr(&host, "supported_features").as_u64().unwrap() & 1, 0);

    service(&host, &id, "media_seek", json!({ "seek_position": 75 })).await.unwrap();
    assert_eq!(r.calls("Seek")[0]["Target"], "0:01:15");
    assert_eq!(r.calls("Seek")[0]["Unit"], "REL_TIME");
    service(&host, &id, "shuffle_set", json!({ "shuffle": true })).await.unwrap();
    assert_eq!(r.calls("SetPlayMode")[0]["NewPlayMode"], "SHUFFLE");
    wait_for("bland", || (attr(&host, "shuffle") == true).then_some(())).await;
    let e = service(&host, &id, "repeat_set", json!({ "repeat": "one" })).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "not_supported");

    service(&host, &id, "media_play_pause", json!({})).await.unwrap();
    wait_for("pause", || (state(&host) == "paused").then_some(())).await;
    assert_eq!(r.calls("Pause").len(), 1);

    // the remote control starts it again; the event says so
    r.remote("PLAYING").await;
    wait_for("afspiller igen", || (state(&host) == "playing").then_some(())).await;

    let e = service(&host, &id, "turn_on", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "not_supported");
    let e = service(&host, &id, "media_seek", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "invalid_argument");

    let d = host.call("diagnostics", json!({ "entry": entry(&r) })).await.unwrap();
    let subs = &d["devices"][0]["status"]["subscriptions"];
    assert_eq!(subs.as_array().unwrap().len(), 2);
    assert!(subs.as_array().unwrap().iter().all(|s| s["events_arrive"] == true), "{subs}");

    host.call("unload_entry", json!({ "entry_id": "e1" })).await.unwrap();
}

#[tokio::test]
async fn polls_when_events_do_not_arrive() {
    // subscriptions are accepted, but no event ever comes, as when
    // Homewright runs in Docker without host networking
    let r = FakeRenderer::start(false).await;
    let host = start("poll").await;
    host.call("setup_entry", json!({ "entry": entry(&r) })).await.unwrap();
    wait_for("tilstand", || (state(&host) == "idle").then_some(())).await;
    {
        let mut st = r.state.lock().unwrap();
        st.uri = "http://nas.lan/film.mp4".into();
        st.transport = "PLAYING".into();
        st.position = "0:00:42".into();
    }
    wait_for("afspiller", || (state(&host) == "playing").then_some(())).await;
    assert_eq!(attr(&host, "media_content_type"), "video");
    assert_eq!(attr(&host, "media_title"), "film.mp4");
    assert_eq!(attr(&host, "media_position"), 42.0);
    let d = host.call("diagnostics", json!({ "entry": entry(&r) })).await.unwrap();
    let subs = d["devices"][0]["status"]["subscriptions"].as_array().unwrap().clone();
    assert!(subs.iter().all(|s| s["subscribed"] == true && s["events_arrive"] == false), "{subs:?}");
}

#[tokio::test]
async fn unavailable_and_back() {
    let r = FakeRenderer::start(true).await;
    let host = start("away").await;
    host.call("setup_entry", json!({ "entry": entry(&r) })).await.unwrap();
    wait_for("tilstand", || (state(&host) == "idle").then_some(())).await;
    let id = host.entity(UID);

    r.down.store(true, Ordering::SeqCst);
    wait_for("utilgængelig", || (state(&host) == "unavailable").then_some(())).await;
    let e = service(&host, &id, "media_play", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "unavailable");

    r.down.store(false, Ordering::SeqCst);
    r.state.lock().unwrap().volume = 33;
    wait_for("tilbage", || (state(&host) == "idle").then_some(())).await;
    wait_for("lydstyrke", || (attr(&host, "volume_level") == 0.33).then_some(())).await;
}
