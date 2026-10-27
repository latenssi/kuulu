# Kuulu – toteutussuunnitelma

Oct 27, 2026 · @Lauri Junkkari

## Nimi ja tavoite

**Kuulu** on avoimen lähdekoodin puhesovellus pelaajille: Mumblen latenssi ja Discordin helppous ilman tilejä tai keskitettyä palvelua. Tavoite on noin 45 ms suusta korvaan plus verkon yhdensuuntainen viive, mitattuna ja julkisesti raportoituna.

Nimi tulee kysymyksestä ”Kuuluuko?”, jonka jokainen puhesovelluksen käyttäjä on joskus kysynyt, ja kuulu tarkoittaa myös tunnettua. Se on lyhyt ja helppo lausua myös englanniksi.

Crate-nimi kuulu on vapaa crates.io:ssa (tarkistettu 27.10.2026). GitHub-käyttäjänimi kuulu on varattu, joten organisaatioksi tarvitaan esimerkiksi kuulu-voice.

Tavaramerkkihaku (EUIPO, USPTO) ja verkkotunnus tehdään ennen ensimmäistä julkista julkaisua.

Kuulu v1 **ei** sisällä tekstichattia, botteja, videota, selainclienttiä, mobiilisovellusta eikä keskitettyä palvelua.

## Periaatteet ja v1-rajaus

Jokainen päätös arvioidaan ensin sen vaikutuksella viiveeseen. Kun jokin ominaisuus lisäisi äänipolulle puskurointia, se tehdään valinnaiseksi ja viive näytetään käyttäjälle.

- **Latenssi ensin.** Mikään muutos ei saa kasvattaa latenssibudjettia ilman mittausta ja perustelua.
- **Mitattava.** Viive mitataan automaattisesti ja näytetään käyttäjälle koko ajan.
- **Yksinkertainen.** Liittyminen kutsulinkillä ilman tiliä. Palvelin on yksi binääri tai yksi Docker-komento.
- **Yksityinen.** Ei telemetriaa oletuksena. Palvelin ei tallenna ääntä eikä kirjaa sisältöä.
- **Ajantasainen.** Allekirjoitetut automaattiset päivitykset ovat mukana ensimmäisestä julkaisusta asti.

**v1 sisältää:** natiiviclientin Windowsille, macOS:lle ja Linuxille; itse hostattavan palvelimen; kanavat; push-to-talkin ja puheentunnistuksen (VAD); mykistyksen ja kuulokkeiden mykistyksen; käyttäjäkohtaisen äänenvoimakkuuden; kutsulinkit; automaattisen päivityksen; latenssinäytön.

**Myöhemmin:** selainclient (WebTransport), mobiili, päästä päähän -salaus, tekstichat, overlay peleihin, Windowsin exclusive-tila.

## Arkkitehtuuri

Kuulu on Rust-workspace: palvelin välittää äänipaketteja purkamatta niitä, ja client ajaa ääniputken omassa reaaliaikasäikeessään erillään käyttöliittymästä. Clientin ja palvelimen välillä on yksi QUIC-yhteys, jossa ohjausviestit kulkevat luotettavassa streamissa ja ääni epäluotettavina datagrammeina.

&#91;embedded content: arkkitehtuuri · client, palvelin ja päivitykset\]

Käyttöliittymä vain näyttää tilan ja lähettää komentoja. Ääni kulkee äänilaitteelta Opus-kehyksinä palvelimelle, joka välittää paketit muille kanavan jäsenille purkamatta niitä.

| Osa | Valinta | Peruste |
| --- | --- | --- |
| Kieli | Rust (stable) | Ei roskienkeruutaukoja äänisäikeessä; sama kieli clientissä ja palvelimessa |
| Async | tokio | Vakiintunut, quinn rakentuu sen päälle |
| Kuljetus | QUIC: quinn (datagrammit + streamit) | TLS 1.3, yksi UDP-portti, uudelleenlähettämättömät datagrammit äänelle |
| Koodekki | Opus (libopus) | 48 kHz, 10 ms kehykset |
| Audio-I/O | cpal | WASAPI, CoreAudio, ALSA/PipeWire |
| Kohinanpoisto | nnnoiseless | RNNoise Rustilla, valinnainen |
| Säikeiden välillä | rtrb | Lukoton SPSC-rengaspuskuri |
| Ohjausviestit | protobuf (prost) | Eteenpäin yhteensopiva, toimii myöhemmin myös TypeScriptissä |
| Pysyvä data (palvelin) | SQLite (rusqlite) + TOML-konfiguraatio | Ei erillistä tietokantapalvelua |
| Client-kuori | Tauri 2 + Svelte + TypeScript | Kevyt, valmis päivitin, Rust-ydin suoraan käytössä |
| Avainten säilytys | keyring | Käyttöjärjestelmän avainnippu |
| CI ja julkaisu | GitHub Actions + tauri-action | Kaikki alustat samasta putkesta |

Repon rakenne:

```
kuulu/
  crates/
    kuulu-proto/    # viestit, pakettiotsake, protokollaversiot
    kuulu-audio/    # capture, Opus, jitter buffer, miksaus, playback
    kuulu-client/   # QUIC-yhteys ja sessio clientin puolella
    kuulu-server/   # palvelinbinääri
    kuulu-bench/    # latenssimittaus, verkkoemulaattori, loopback-testit
  apps/
    desktop/        # Tauri-sovellus (src-tauri + Svelte-UI)
  deploy/           # Dockerfile, docker-compose, systemd-esimerkki
  docs/             # PLAN.md (tämä), protokolla, julkaisuohje
  .github/workflows/
```

## Ääniputki ja latenssibudjetti

Budjetti on 43,5 ms suusta korvaan, kun verkon yhdensuuntainen viive on 11 ms (RTT 22 ms). Jokainen rivi on oma mitattava vaiheensa, ja client näyttää ne asetuksissa.

| Vaihe | Viive (ms) | Miten |
| --- | --- | --- |
| Capture-puskuri | 5 | 240 näytettä, 48 kHz |
| Opus-koodaus | 12,5 | 10 ms kehys + 2,5 ms look-ahead (low-delay-tila) |
| Verkko | 11 | Yhdensuuntainen, palvelimen kautta |
| Jitter buffer | 10 | Adaptiivinen, tavoite yksi kehys |
| Playback-puskuri | 5 | 240 näytettä, 48 kHz |
| **Yhteensä** | **43,5** |  |

Kohinanpoisto lisää noin 10 ms, joten se on oletuksena pois päältä ja sen viive näytetään erikseen.

**Lähetys:**

1. cpal-capture-callback kirjoittaa 48 kHz mono f32 -näytteet rtrb-puskuriin.
2. Äänisäie kokoaa 480 näytteen kehykset.
3. Valinnainen kohinanpoisto (nnnoiseless).
4. PTT- tai VAD-portti päättää, lähetetäänkö kehys.
5. Opus-koodaus 10 ms kehyksinä.
6. Paketti lähetetään QUIC-datagrammina heti, ilman jonoa.

**Vastaanotto:**

1. Datagrammi ohjataan lähettäjäkohtaiseen jitter bufferiin sekvenssinumeron ja aikaleiman mukaan.
2. Opus-purku; puuttuva kehys paikataan redundanssista tai Opuksen PLC:llä.
3. Miksaus: lähettäjäkohtainen vahvistus, summa ja limitteri.
4. Tulos rtrb-puskuriin, josta cpal-playback-callback lukee.

**Jitter buffer:** tavoite yksi kehys, enintään kuusi (60 ms). Puskuri kasvaa heti myöhästyneen paketin jälkeen ja kutistuu hitaasti, yksi kehys kahden sekunnin välein, pudottamalla kehyksen hiljaisuuden aikana. Sama mekanismi korjaa äänilaitteiden kellojen ajautumisen.

**Pakettihäviöt:** Opuksen sisäinen FEC toimii vain SILK- ja hybriditilassa, ei low-delay-tilassa. Siksi oletuksena jokainen paketti kantaa myös edellisen kehyksen (redundanssi, noin 2 × 32 kbit/s). Vaihtoehto on VOIP-tila ja sisäinen FEC, joka lisää look-aheadia 4 ms. Valinta tehdään mittaamalla vaiheessa M3.

**Reaaliaikasäännöt äänisäikeessä ja callbackeissa:** ei muistinvarausta, ei lukkoja, ei I/O:ta eikä lokitusta. Säikeiden välinen data kulkee vain lukottomilla rengaspuskureilla. Säännön noudattaminen varmistetaan testeissä (assert\_no\_alloc).

## Verkko, protokolla ja versiointi

Jokaisella clientillä on yksi QUIC-yhteys palvelimeen (oletuksena UDP 7700). Ohjausviestit ovat protobufia pituusetuliitteellä yhdessä kaksisuuntaisessa streamissa, ja ääni kulkee datagrammeina. Clientit eivät puhu toisilleen suoraan, joten NAT-läpäisyä ei tarvita.

**Äänipaketin otsake** (kiinteä binäärimuoto, little-endian):

| Kenttä | Koko | Sisältö |
| --- | --- | --- |
| version | u8 | Pakettimuodon versio |
| flags | u8 | PTT päättyi, redundanssi mukana |
| sender | u32 | Lähettäjän session-id; palvelin kirjoittaa sen, clientin arvo ohitetaan |
| seq | u16 | Kehyksen järjestysnumero, kiertää ympäri |
| timestamp | u32 | Näytelaskuri 48 kHz:llä |
| payload | loput | Opus-kehys (+ edellinen kehys, jos redundanssi) |

Palvelin ei pura ääntä. Se tarkistaa lähettäjän, kirjoittaa sender-kentän ja välittää paketin muille saman kanavan jäsenille.

**Vanha ääni on pahempi kuin hävinnyt ääni.** Datagrammien lähetysjono pidetään yhden kehyksen mittaisena, ja vanhentunut kehys pudotetaan ennemmin kuin jonotetaan. Sama sääntö pätee palvelimen lähtevään liikenteeseen.

**Kättely:**

1. Client avaa yhteyden ALPN-tunnisteella `kuulu/<pääversio>`.
2. Client lähettää `Hello`: protokollan versio, pienin tuettu versio, sovelluksen versio, ominaisuuslista, julkinen avain ja kutsutunniste.
3. Palvelin vastaa `Welcome` (session-id, palvelimen versio, kanavat ja jäsenet) tai `Reject` syineen: liian vanha, liian uusi, kutsu vanhentunut, bannattu.

**Versiointi:** palvelin tukee nykyistä ja edellistä protokollan pääversiota. Pienet lisäykset kulkevat ominaisuuslistan kautta, ja tuntemattomat protobuf-kentät ohitetaan. Kun palvelin on liian vanha, client näyttää selkeän viestin eikä yritä yhdistää uudelleen silmukassa.

## Palvelin

`kuulu-server` on yksi binääri, joka jaetaan myös Docker-imagena (`ghcr.io/<org>/kuulu-server:<versio>`). Käynnistykseen riittää yksi komento ja yksi avoin UDP-portti.

- **Konfiguraatio:** `kuulu.toml`: palvelimen nimi, portti, kanavat ja ylläpitäjien julkiset avaimet. Ensimmäinen käynnistys luo oletuskonfiguraation ja tulostaa ylläpitäjän kutsulinkin.
- **Tila:** yhdistetyt sessiot ja kanavat pidetään muistissa. SQLiteen tallentuvat tunnetut identiteetit, roolit, bannit ja kutsut.
- **Kutsut:** ylläpitäjä luo kutsun clientistä tai komentoriviltä (`kuulu-server invite --uses 10 --expires 7d`). Kutsulla on satunnainen tunniste, voimassaoloaika ja käyttökertojen enimmäismäärä.
- **Oikeudet v1:** ylläpitäjä ja jäsen. Ylläpitäjä voi luoda ja poistaa kanavia, siirtää, potkia ja bannata käyttäjiä.
- **Yksityisyys:** ei äänen tallennusta eikä sisällön lokitusta. Lokeihin kirjataan vain yhteystapahtumat.
- **Päivittäminen:** ylläpitäjä vaihtaa Docker-tagin. Palvelin voi valinnaisesti tarkistaa uusimman version ja kertoa siitä ylläpitäjän clientille.
- **Mitoitustavoite:** 100 samanaikaista käyttäjää yhden vCPU:n virtuaalipalvelimella. Tavoite varmistetaan kuormitustestillä vaiheessa M6.

## Client ja käyttöliittymä

Client on Tauri 2 -sovellus, jossa ääni ja verkko ajetaan Rust-ytimessä ja käyttöliittymä WebView'ssä. Ääni ei koskaan kulje käyttöliittymän kautta. UI saa tilan Tauri-eventeinä ja lähettää komentoja, kuten mykistyksen tai kanavan vaihdon.

Näkymät noudattavat [mockupia](https://claude.ai/artifact/VX6zrxTnwTBNUzJEf3LXJb):

1. **Puhekanava:** palvelimet ja kanavat sivupalkissa, osallistujat korteissa (puhuu, mykistetty, RTT, äänenvoimakkuus), alapalkissa omat kontrollit ja latenssi suusta korvaan.
2. **Liittyminen:** kutsulinkki avaa näkymän, jossa valitaan nimi, mikrofoni ja puhetapa. Tiliä ei ole.
3. **Asetukset:** latenssibudjetti vaiheittain, laitteet ja puskurikoko, kehyskoko, jitter buffer, kohinanpoisto ja päivitykset.

**Kutsulinkit:** muoto `kuulu://<host>:<portti>/join?t=<kutsu>&fp=<sertifikaatin SHA-256>`. Linkki toimii myös https-osoitteena, joka ohjaa sovellukseen tai lataussivulle. Deep link -käsittely tehdään tauri-plugin-deep-linkillä.

**Push-to-talk:** globaali näppäin tai hiiren lisänäppäin. Windowsilla ja X11:llä tämä onnistuu suoraan. macOS vaatii saavutettavuusoikeuden, ja Waylandilla käytetään GlobalShortcuts-portaalia. Jos globaali näppäin ei ole käytettävissä, client tarjoaa VAD:n ja kertoo syyn.

**Paikallinen data:** asetukset JSON-tiedostona sovelluksen konfiguraatiokansiossa; identiteettiavain käyttöjärjestelmän avainnipussa.

## Automaattinen päivitys ja julkaisuputki

Päivitys rakennetaan heti ensimmäiseen versioon (vaihe M1), jotta jokainen myöhempi julkaisu leviää kaikille itsestään. Client käyttää Tauri 2:n [updater-pluginia](https://v2.tauri.app/plugin/updater/), joka tarkistaa allekirjoituksen ennen asennusta ja voi lukea päivitystiedot staattisesta JSON-tiedostosta GitHub Releasesissa.

**Päivityksen kulku clientissä:**

1. Tarkistus käynnistyksessä ja kuuden tunnin välein.
2. Lataus taustalla.
3. Asennus vasta, kun käyttäjä ei ole puhelussa. Puhelun aikana UI näyttää vain huomaamattoman ilmoituksen.
4. Uudelleenkäynnistys palauttaa käyttäjän samalle palvelimelle ja kanavalle.

**Kanavat:** stable ja beta omina päivitysosoitteinaan. Beta-julkaisut ovat `vX.Y.Z-beta.N`-tageja ja pysyvät betassa vähintään kolme päivää ennen stablea.

**Julkaisuputki (GitHub Actions):**

1. Tagi `vX.Y.Z` käynnistää buildin Windowsille, macOS:lle (universal) ja Linuxille (AppImage).
2. Binäärit allekirjoitetaan käyttöjärjestelmän tasolla ja päivitysavaimella.
3. Putki luo GitHub Releasen luonnoksena ja päivitystiedoston.
4. Palvelimen Docker-image rakennetaan samasta tagista (`X.Y.Z`, `latest` tai `beta`).
5. Ihminen julkaisee luonnoksen.

**Allekirjoitusavaimet:**

- Tauri-päivitysavain on projektin arvokkain salaisuus. Se säilytetään GitHub Actionsin salaisuutena, ja siitä on offline-varmuuskopio kahdessa paikassa. Vuotanut avain antaa kenelle tahansa reitin kaikkien käyttäjien koneille. Kadonnut avain estää päivitykset nykyisille asennuksille.
- Windows: Authenticode-allekirjoitus SmartScreen-varoitusten välttämiseksi.
- macOS: Developer ID -allekirjoitus ja notarisointi (maksullinen Apple Developer -jäsenyys).

**Virheellinen julkaisu** korjataan julkaisemalla uudempi versio, ei palaamalla vanhaan.

## Identiteetti ja turvallisuus

Käyttäjän identiteetti on clientin ensimmäisellä käynnistyksellä luoma ed25519-avainpari. Salasanoja ja tilejä ei ole. Palvelin tunnistaa käyttäjän julkisesta avaimesta, ja näyttönimen saa valita vapaasti.

- **Kuljetus:** QUIC ja TLS 1.3. Itse allekirjoitettu palvelinsertifikaatti on sallittu, koska kutsulinkki kantaa sen SHA-256-sormenjäljen ja client kiinnittää sen. Verkkotunnuksellinen palvelin voi käyttää myös Let's Encrypt -sertifikaattia.
- **Kutsut:** 128-bittinen satunnainen tunniste, voimassaoloaika ja käyttökertojen enimmäismäärä.
- **Ei päästä päähän -salausta v1:ssä.** Palvelin näkee Opus-paketit, vaikka ei pura eikä tallenna niitä. Tämä kerrotaan käyttäjille avoimesti, ja E2E-salaus on myöhemmän version tavoite.
- **Väärinkäytön esto:** palvelin rajoittaa jokaisen yhteyden datagrammimäärää (oletus 120 pakettia sekunnissa) ja hylkää paketit, joiden lähettäjä ei kuulu kanavaan.
- **Riippuvuudet:** cargo-deny ja cargo-audit ajetaan CI:ssä jokaiselle muutokselle.
- **Päivitykset** on allekirjoitettu (katso edellinen osio).

## Mittaaminen ja testaus

Latenssi mitataan kolmella tasolla, jotta regressiot jäävät kiinni ennen julkaisua ja vertailuluvut voi julkaista.

1. **Algoritminen viive CI:ssä.** `kuulu-bench` ajaa koko ääniputken simuloidulla kellolla ilman äänilaitteita: syöttää klikin, kulkee koodauksen, simuloidun verkon ja jitter bufferin läpi ja mittaa viiveen näytteen tarkkuudella. CI epäonnistuu, jos tulos ylittää budjetin.
2. **Verkkoemulaattori.** `kuulu-bench` sisältää UDP-välityspalvelimen, joka lisää viivettä, jitteriä, häviötä ja uudelleenjärjestystä toistettavasti. Testit ajetaan profiileilla: lähiverkko, kotiyhteys (1 % häviö, 5 ms jitter) ja huono mobiiliyhteys (5 % häviö, 30 ms jitter).
3. **Laitteistomittaus suusta korvaan.** Kaksi konetta, loopback-kaapeli ja klikkisignaali. Samalla järjestelyllä mitataan Mumble, TeamSpeak ja Discord, ja menetelmä julkaistaan tulosten kanssa.

Client näyttää jatkuvasti RTT:n, pakettihäviön, jitter bufferin syvyyden ja arvion viiveestä suusta korvaan.

**Muut testit:** yksikkötestit jitter bufferille, pakettiotsakkeelle ja kättelylle; pakettien jäsennyksen fuzzaus (cargo-fuzz); integraatiotesti, jossa palvelin ja kaksi headless-clienttiä ajetaan samassa prosessissa; kuormitustesti, jossa 100 simuloitua clienttiä puhuu yhtä aikaa.

## Toteutusvaiheet

Toteutus etenee seitsemässä vaiheessa, ja jokainen päättyy toimivaan ja testattuun tilaan. Automaattinen päivitys rakennetaan toisena vaiheena, ennen ääntä, jotta jokainen testiversio päivittyy testaajille itsestään.

### M0 – Repo ja CI

- [x] Cargo-workspace ja crate-rungot repon rakenteen mukaan
- [x] CI: fmt, clippy (`-D warnings`) ja testit Linuxilla, Windowsilla ja macOS:llä
- [x] cargo-deny ja cargo-audit
- [x] LICENSE, README, CLAUDE.md ja docs/PLAN.md

**Valmis kun:** tyhjä workspace läpäisee CI:n kaikilla kolmella alustalla.

### M1 – Itsestään päivittyvä kuori

- [x] Tauri 2 + Svelte -sovellus, joka näyttää versionumeron
- [x] Updater-plugin: julkinen avain konfiguraatiossa, stable- ja beta-osoitteet
- [ ] Julkaisuworkflow: tagista allekirjoitettu build kolmelle alustalle, release-luonnos ja päivitystiedosto
- [x] Julkaisuohje docs/RELEASING.md

**Valmis kun:** asennettu v0.0.1 päivittyy itsestään versioon v0.0.2 Windowsilla, macOS:llä ja Linuxilla.

### M2 – Ääniputki paikallisesti

- [x] kuulu-audio: capture, kehystys, Opus, jitter buffer, miksaus ja playback
- [x] Komento `kuulu-bench loopback`: mikrofoni → putki → kaiuttimet ilman verkkoa
- [x] Viivetesti simuloidulla kellolla
- [x] assert\_no\_alloc äänicallbackeihin debug-buildeissa

**Valmis kun:** viive ilman verkkoa on enintään 32,5 ms, ja 10 minuutin ajo sujuu ilman katkoja tai rätinää.

### M3 – Palvelin ja verkko

- [x] kuulu-proto: protobuf-viestit, pakettiotsake ja versiot
- [x] kuulu-server: QUIC, kättely, yksi kanava ja datagrammien välitys
- [x] kuulu-client: yhteys, sessio ja uudelleenyhdistys
- [x] Verkkoemulaattori ja häviöprofiilit
- [ ] Mittaus: redundanssi vai Opus FEC; päätös kirjataan tähän dokumenttiin

**Valmis kun:** kaksi headless-clienttiä puhuu palvelimen kautta, ja kotiyhteysprofiililla puhe pysyy ymmärrettävänä jitter bufferin pysyessä enintään kahdessa kehyksessä.

### M4 – Puheominaisuudet

- [ ] Useat kanavat ja kanavan vaihto
- [ ] Push-to-talk globaalilla näppäimellä ja VAD
- [ ] Mykistys, kuulokkeiden mykistys ja käyttäjäkohtainen äänenvoimakkuus
- [ ] Ylläpitäjän toiminnot: kanavien hallinta, siirto, potku ja banni

**Valmis kun:** neljä käyttäjää puhuu samassa kanavassa, ja kaikki kontrollit toimivat kolmella alustalla.

### M5 – Käyttöliittymä ja liittyminen

- [ ] Mockupin kolme näkymää
- [ ] Identiteettiavain käyttöjärjestelmän avainnippuun
- [ ] Kutsulinkit, sertifikaatin kiinnitys ja deep link
- [ ] Latenssinäyttö ja asetusten latenssibudjetti

**Valmis kun:** uusi käyttäjä pääsee kutsulinkistä puhumaan alle minuutissa ilman tiliä.

### M6 – Mittaus, kovennus ja 0.1-beta

- [ ] Laitteistomittaus ja vertailu Mumbleen, TeamSpeakiin ja Discordiin
- [ ] Fuzzaus ja kuormitustesti 100 clientillä
- [ ] Docker-image, docker-compose ja palvelimen asennusohje
- [ ] Julkinen 0.1.0-beta

**Valmis kun:** mitattu viive suusta korvaan pysyy budjetissa lähiverkossa, ja palvelin kestää 100 samanaikaista käyttäjää yhdellä vCPU:lla.

## Ohjeet Claude Codelle

Lataa tämä dokumentti Markdownina repoon tiedostoksi `docs/PLAN.md`, luo repon juureen alla oleva `CLAUDE.md` ja anna Claude Coden toteuttaa yksi vaihe kerrallaan.

1. Aloita jokainen vaihe plan modessa. Claude Code lukee suunnitelman ja ehdottaa toteutusjärjestyksen ennen koodia.
2. Hyväksymiskriteerit kirjoitetaan testeiksi ensin.
3. Pienet commitit, ja valmiit tehtävät ruksitaan PLAN.md:ssä.
4. Protokollan, latenssibudjetin ja riippuvuuksien muutokset hyväksyt sinä.

Ensimmäinen kehote: *Lue docs/PLAN.md ja CLAUDE.md. Toteuta vaihe M0. Ehdota ensin suunnitelma ja odota hyväksyntääni.*

```markdown
# Kuulu

Open-source, low-latency voice chat for gamers: a native desktop client
(Tauri 2 + Rust) and a self-hosted server. The full plan is in docs/PLAN.md.
Read it before starting any work.

## Ground rules
- Write all code, comments, commit messages and docs in English.
- Work one milestone at a time (M0-M6 in docs/PLAN.md). Propose a plan
  before writing code. Check off tasks in PLAN.md when they are done.
- Latency is the product. Never add buffering to the audio path without
  measuring it and updating the latency budget in PLAN.md.
- Audio callbacks and the audio thread: no allocation, no locks, no I/O,
  no logging. Pass data between threads only through rtrb ring buffers.
- Stale audio is worse than lost audio: never queue voice datagrams,
  drop old frames instead.
- Ask before changing the wire protocol (kuulu-proto), the latency budget,
  or adding a dependency.
- Never commit secrets or signing keys.

## Commands
- Build: cargo build --workspace
- Test: cargo test --workspace
- Lint: cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
- Desktop app: cd apps/desktop && npm install && npm run tauri dev
- Latency check: cargo run -p kuulu-bench -- latency

## Layout
- crates/kuulu-proto: messages, packet header, protocol versions
- crates/kuulu-audio: capture, Opus, jitter buffer, mixing, playback
- crates/kuulu-client: QUIC session, client side
- crates/kuulu-server: server binary
- crates/kuulu-bench: latency measurement and network emulator
- apps/desktop: Tauri app with Svelte UI
```

## Avoimet kysymykset

- [x] **Lisenssi:** AGPL-3.0-only (päätetty 27.10.2026). Pitää myös hostatut johdannaiset avoimina.
- [ ] **Nimi:** tavaramerkkihaku, verkkotunnus ja GitHub-organisaatio.
- [ ] **Pakettihäviöt:** redundanssi vai Opuksen sisäinen FEC; ratkaistaan mittaamalla vaiheessa M3.
- [ ] **Koodiallekirjoitus:** Windows- ja macOS-sertifikaattien kustannukset ja kuka hallinnoi avaimia.
- [ ] **Windowsin exclusive-tila:** vaatii oman WASAPI-toteutuksen cpalin rinnalle; v1 vai myöhemmin.
- [ ] **Päästä päähän -salaus:** menetelmä ja aikataulu v1:n jälkeen.
