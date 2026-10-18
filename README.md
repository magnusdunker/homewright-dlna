# DLNA til Homewright

En ekstern integration til [Homewright](https://github.com/magnusdunker/homewright), der styrer DLNA-afspillere (UPnP MediaRenderer) som medieafspillere: smart-tv, receivere, netværkshøjttalere og programmer som Kodi og BubbleUPnP. Den taler UPnP direkte med afspillerne på det lokale netværk.

## Installation

I Homewright: **Indstillinger → Integrationer → Fra GitHub**. Indsæt

```
https://github.com/magnusdunker/homewright-dlna
```

og tryk **Installér**. Tilføj derefter **DLNA** under **Tilføj integration**. Formularen viser de afspillere, der blev fundet på netværket. Står alle valgt, kommer afspillere, der dukker op senere, også med. Når integrationen er installeret, foreslår Homewright den også selv, når den finder en afspiller.

Der er kun én opsætning, og den dækker alle afspillere. Valget kan ændres under integrationens indstillinger.

Sonos-højttalere er også DLNA-afspillere, men de bliver ikke tilføjet, når alle er valgt, fordi de har deres egen integration. Vælg dem enkeltvis, hvis du vil have dem med her.

### Afspillere, som SSDP ikke finder

Afspillerne findes med SSDP, som kun virker på samme netværk. Står en afspiller på et andet VLAN, eller kører Homewright i Docker uden host-netværk, så skriv URL'en til dens enhedsbeskrivelse under **Ekstra adresser**, f.eks. `http://192.168.20.15:9197/dmr`. Flere adresser adskilles med komma. Adressen prøves, når du gemmer, og integrationen prøver igen hvert 30. sekund, hvis afspilleren er væk ved opstart.

URL'en står i afspillerens svar på en SSDP-søgning (`LOCATION`). Programmer som "UPnP Inspector" eller `gssdp-discover` viser den.

## Hvad du får

Hver afspiller bliver en enhed i Homewright med én medieafspiller. Navnet er det, afspilleren selv oplyser. Et tv får enhedsklassen `tv`.

Tilstanden er:

- **playing**, **paused** og **buffering** efter afspillerens AVTransport
- **idle**, når den er stoppet eller intet har indlæst
- **unavailable**, når den ikke svarer, f.eks. fordi tv'et er slukket

DLNA har ingen tænd og sluk, så `turn_on` og `turn_off` findes ikke.

Attributterne følger Home Assistant: `volume_level`, `is_volume_muted`, `media_content_id`, `media_content_type`, `media_title`, `media_artist`, `media_album_name`, `media_album_artist`, `media_track`, `media_duration`, `media_position`, `media_position_updated_at`, `entity_picture`, `shuffle`, `repeat` og `supported_features`. Titel, kunstner og omslag kommer fra afspillerens DIDL-Lite-metadata. Mangler titlen, bruges filnavnet fra URL'en.

Tjenesterne er:

- `media_play`, `media_pause`, `media_play_pause`, `media_stop` og `media_seek`
- `media_next_track` og `media_previous_track`
- `volume_set`, `volume_mute`, `volume_up` og `volume_down` (5 % af skalaen ad gangen)
- `shuffle_set` og `repeat_set`, når afspilleren har de afspilningstilstande
- `play_media` afspiller en URL:

```yaml
action: media_player.play_media
target:
  entity_id: media_player.stue_tv
data:
  media_content_id: http://nas.lan/musik/sang.mp3
  media_content_type: music        # eller en MIME-type som audio/mpeg
  extra:
    title: Sang
    thumb: http://nas.lan/musik/cover.jpg
```

Er `media_content_type` ikke en MIME-type, gættes den ud fra URL'ens endelse. Integrationen slår MIME-typen op i afspillerens liste over formater (`GetProtocolInfo`) og bruger dens egne DLNA-flag, så afspillere, der er kræsne med `protocolInfo`, tager imod mediet. Den stopper det, der spiller, sætter den nye URL og trykker afspil.

`supported_features` følger det, afspilleren kan lige nu: står der intet i den, kan den ikke afspille eller sætte på pause, men den kan godt få en URL med `play_media`.

### Hændelser og spørgsmål

Integrationen abonnerer på afspillerens hændelser (UPnP GENA). Afspilleren sender selv sin tilstand til en lille HTTP-server i integrationen, når den ændrer sig. Indtil den første hændelse er kommet, og hvis den aldrig kommer, spørger integrationen afspilleren hvert 5. sekund. Når hændelserne kommer, spørger den kun hvert 30. sekund for at opdage en afspiller, der er forsvundet.

Hændelserne kræver, at afspilleren kan nå Homewright. I Docker uden host-netværk, eller bag en firewall, kommer de ikke frem, og så klarer spørgsmålene det alene. Under **Port til hændelser** kan du sætte en fast port, hvis den skal åbnes i en firewall. Diagnosticeringen viser for hver afspiller, om hændelserne når frem.

Svarer en afspiller ikke to gange i træk, bliver den utilgængelig. Integrationen prøver den igen hvert 30. sekund og med det samme, når SSDP ser den igen. SSDP spørges hvert minut, så en afspiller, der skifter IP-adresse, bliver fundet på den nye.

Integrationen husker, hvor SSDP sidst fandt hver afspiller. Er et tv slukket, når Homewright starter, findes medieafspilleren alligevel og står som utilgængelig, til tv'et tændes.

## Udvikling

Bygget i Rust på SDK'en `thehome-plugin` fra [homewright-example-integration-rust](https://github.com/magnusdunker/homewright-example-integration-rust).

- `src/ssdp.rs`: SSDP-søgning på alle netkort
- `src/upnp.rs`: enhedsbeskrivelse, servicebeskrivelser (SCPD), SOAP og GENA-abonnementer
- `src/events.rs`: HTTP-serveren, der tager imod hændelserne
- `src/device.rs`: én afspiller: abonnementer, spørgsmål, tjenester og genforbindelse
- `src/status.rs`: fra afspillerens tilstand til tilstand og attributter, og DIDL-Lite
- `src/xml.rs`: en lille XML-parser
- `src/lib.rs`: opsætningen og forbindelsen til Homewright
- `tests/`: tests mod en falsk afspiller i `tests/fake` og en falsk Homewright i `tests/host`

```
cargo test
cargo build --release
```

En ny version udgives med et tag, f.eks. `git tag v0.2.0 && git push origin v0.2.0`. Workflowet bygger til Windows, Linux (x86_64 og arm64, statisk musl) og macOS og lægger binærerne på releasen sammen med `homewright.json`.

## Licens

MIT
