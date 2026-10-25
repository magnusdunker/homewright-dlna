# DLNA for HomewrightHQ

An external integration for [HomewrightHQ](https://github.com/magnusdunker/homewrighthq) that controls DLNA players (UPnP MediaRenderer) as media players: smart TVs, receivers, network speakers and programs such as Kodi and BubbleUPnP. It talks UPnP directly with the players on the local network.

## Installation

In HomewrightHQ: **Settings → Integrations → From GitHub**. Paste

```
https://github.com/magnusdunker/homewrighthq-dlna
```

and press **Install**. Then add **DLNA** under **Add integration**. The form shows the players that were found on the network. If all are selected, players that show up later are included too. Once the integration is installed, HomewrightHQ also suggests it by itself when it finds a player.

There is only one entry, and it covers all players. The selection can be changed under the integration's options.

Sonos speakers are DLNA players too, but they are not added when all are selected, because they have their own integration. Select them one by one if you want them here.

### Players that SSDP does not find

The players are found with SSDP, which only works on the same network. If a player is on another VLAN, or HomewrightHQ runs in Docker without host networking, enter the URL of its device description under **Ekstra adresser** (extra addresses), e.g. `http://192.168.20.15:9197/dmr`. Separate several addresses with commas. The address is tried when you save, and the integration tries again every 30 seconds if the player is gone at startup.

The URL is in the player's reply to an SSDP search (`LOCATION`). Programs such as "UPnP Inspector" or `gssdp-discover` show it.

## What you get

Each player becomes a device in HomewrightHQ with one media player. The name is the one the player reports itself. A TV gets the device class `tv`.

The state is:

- **playing**, **paused** and **buffering** according to the player's AVTransport
- **idle** when it is stopped or has nothing loaded
- **unavailable** when it does not respond, e.g. because the TV is switched off

DLNA has no power on and off, so `turn_on` and `turn_off` do not exist.

The attributes follow Home Assistant: `volume_level`, `is_volume_muted`, `media_content_id`, `media_content_type`, `media_title`, `media_artist`, `media_album_name`, `media_album_artist`, `media_track`, `media_duration`, `media_position`, `media_position_updated_at`, `entity_picture`, `shuffle`, `repeat` and `supported_features`. Title, artist and cover art come from the player's DIDL-Lite metadata. If the title is missing, the file name from the URL is used.

The services are:

- `media_play`, `media_pause`, `media_play_pause`, `media_stop` and `media_seek`
- `media_next_track` and `media_previous_track`
- `volume_set`, `volume_mute`, `volume_up` and `volume_down` (5 % of the scale at a time)
- `shuffle_set` and `repeat_set`, when the player has those play modes
- `play_media` plays a URL:

```yaml
action: media_player.play_media
target:
  entity_id: media_player.stue_tv
data:
  media_content_id: http://nas.lan/musik/sang.mp3
  media_content_type: music        # or a MIME type such as audio/mpeg
  extra:
    title: Sang
    thumb: http://nas.lan/musik/cover.jpg
```

If `media_content_type` is not a MIME type, it is guessed from the URL's extension. The integration looks up the MIME type in the player's list of formats (`GetProtocolInfo`) and uses the player's own DLNA flags, so players that are picky about `protocolInfo` accept the media. It stops what is playing, sets the new URL and presses play.

`supported_features` follows what the player can do right now: if it lists nothing, the player cannot play or pause, but it can still take a URL with `play_media`.

### Events and polling

The integration subscribes to the player's events (UPnP GENA). The player sends its state to a small HTTP server in the integration by itself when it changes. Until the first event has arrived, and if it never arrives, the integration polls the player every 5 seconds. Once events arrive, it only polls every 30 seconds to notice a player that has disappeared.

Events require that the player can reach HomewrightHQ. In Docker without host networking, or behind a firewall, they do not get through, and then polling handles it alone. Under **Port til hændelser** (port for events) you can set a fixed port if it has to be opened in a firewall. The diagnostics show for each player whether events get through.

If a player does not respond twice in a row, it becomes unavailable. The integration tries it again every 30 seconds and straight away when SSDP sees it again. SSDP is queried every minute, so a player that changes IP address is found at the new one.

The integration remembers where SSDP last found each player. If a TV is off when HomewrightHQ starts, the media player is still created and shows as unavailable until the TV is switched on.

## Development

Built in Rust on the `thehome-plugin` SDK from [homewright-example-integration-rust](https://github.com/magnusdunker/homewrighthq-example-integration-rust).

- `src/ssdp.rs`: SSDP search on all network interfaces
- `src/upnp.rs`: device description, service descriptions (SCPD), SOAP and GENA subscriptions
- `src/events.rs`: the HTTP server that receives the events
- `src/device.rs`: one player: subscriptions, polling, services and reconnection
- `src/status.rs`: from the player's status to state and attributes, and DIDL-Lite
- `src/xml.rs`: a small XML parser
- `src/lib.rs`: the setup and the connection to HomewrightHQ
- `tests/`: tests against a fake player in `tests/fake` and a fake HomewrightHQ in `tests/host`

```
cargo test
cargo build --release
```

A new version is released with a tag, e.g. `git tag v0.2.0 && git push origin v0.2.0`. The workflow builds for Windows, Linux (x86_64 and arm64, static musl) and macOS and puts the binaries on the release together with `homewright.json`.

## License

MIT
