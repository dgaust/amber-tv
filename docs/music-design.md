# Amber Music: design

Status: **proposal, nothing built.** Written 2026-11-27 against `amber` @ `38a16d2`.
Scope decision taken 2026-08-14 ("Music was just a tab in a mockup, remove it completely") is what this
reverses: `data/src/browse/mod.rs` says a music kind is to be re-added "in the commit that builds its level,
not before". This document is that level, designed before any of it is written.

## 1. Goals and non-goals

Goals
- Play a Plex music library from the couch, with the remote, on both TVs (OLED65B9PTA webOS 4.10, SM8600PTA).
- Feel like the rest of Amber: white focus plate, amber accent, glass panels, artwork-tinted page wash.
- Music keeps playing while you browse (the one thing that makes it a music player rather than a video
  player with a different poster shape).
- Gapless by default. Crossfade, loudness levelling, lyrics and a visualiser as opt-in extras.

Non-goals for v1
- Editing metadata, tagging, matching, uploading, downloading/offline sync.
- Podcasts, audiobooks, radio streaming from plex.tv (Plex's "Tidal"/online music), photos.
- Casting *to* Amber (it is not a Plex "player" target yet). Remote control from a phone: later.
- Chromecast-style multiroom, equaliser, DSP.
- Background play after leaving Amber (Home key). See open question Q1.

## 2. What Plex gives us (the API we will rely on)

The repo's `docs/plex-openapi.json` already documents most of it. Everything marked *verify* is to be
confirmed against the household server in the spike (milestone M0), not assumed.

| Need | Endpoint |
|---|---|
| Music sections | `/library/sections` entries with `type=artist` (today dropped by `SecKind::from_wire`) |
| Browse | `/library/sections/{id}/all?type=8` artists, `type=9` albums, `type=10` tracks; same sorts/filters/A-Z rail as movies |
| Artist page | `/library/metadata/{artist}` (+ `includePopularLeaves=1` for top tracks), `/children` = albums, `/grandchildren` = tracks, `/hubs/metadata/{id}/related` = similar artists, appearances |
| Album page | `/library/metadata/{album}` + `/children` = tracks (disc number = `parentIndex` on the track) |
| Home hubs | `/hubs/sections/{id}`: Recently Played, Recently Added, Most Played, Rediscover, Random, Mixes; Home's `/hubs` already carries "Recently Added Music" |
| Search | `/hubs/search` already returns `artist`, `album`, `track` hubs; `data/src/search.rs` ignores them today |
| Play queue | `POST /playQueues?type=audio&uri=...&shuffle=&repeat=`; `/playQueues/{id}/items`, `/move`, `/shuffle`, `/unshuffle`, `/reset` |
| Audio bytes | the Part key `/library/parts/{id}/file.{ext}` (direct play); universal audio transcode `/music/:/transcode/universal/start...` for formats the TV cannot take (*verify exact params*) |
| Timeline / played | `POST /:/timeline` every 10 s and on state change (Amber already does this for video), `/:/scrobble` |
| Rating | `PUT /:/rate?key=&rating=` (0-10) |
| Playlists | `/playlists?type=15` (audio: `playlistType=audio`), `/playlists/{id}/items`; create `POST /playlists`; add `PUT /playlists/{id}/items?uri=` |
| Lyrics | a stream of type 4 on the track; `GET /library/streams/{id}.lrc` (synced) or `.txt`. Plex Pass (*verify*) |
| Loudness | `Stream.gain`, `albumGain`, `peak` on the track's audio stream (*verify*); `/library/streams/{id}/levels` per-100 ms loudness (waveform for the scrubber, optional) |
| Sonic similarity / radio | `GET /library/metadata/{id}/nearest`, `/library/sections/{id}/nearest`. Needs Plex Pass and sonic analysis having run (*verify*) |

## 3. Audio playback: the hard part

### 3.1 What exists, and what is in the way
- The engine is video-first. `play-feed/src/ff.rs` treats "no video stream" as a fault
  (`DemuxEffect::NoVideo` -> `FailureKind::NoVideoTrack`). The Starfish `Load` payload built by
  `build_av_payload` describes a video and an audio stream.
- `PlaybackSession` (media/route) is a video concept: ABR, remux vs encode, direct-play planning, intro
  markers, subtitles. None of it applies to a track.
- What we can reuse: the Starfish sink, `ff_pcm.rs` (app-side decode to PCM, already used for FLAC, ALAC,
  LPCM, DTS, MP3, Vorbis, TrueHD), `speed_audio::Resampler`, the feed pump's clocking, the timeline
  reporter, the HTTP fetch layer (`curlio`).
- libpf's `Load` vocabulary is AAC / AC3 / EAC3 / DTS / MP3 / MPEG / OPUS / VORBIS / WMA / PCM. Everything
  else must go in as PCM (`contents.pcmInfo`).

### 3.2 Decision
A separate, small **music engine** (`MusicEngine`, in `plx_media`) that owns one audio-only sink
pipeline and a **continuous PCM feed**. It does not extend `PlaybackSession`.

Why PCM for everything, even MP3/AAC (which the pipeline could take compressed):
- One code path for every format; gapless is just "the next track's samples follow the last one's".
- Crossfade, loudness levelling, fade on pause/sleep and the visualiser are all trivial on PCM and
  impossible on a compressed passthrough.
- Cost: decode CPU on a quad-core ARM TV. FLAC/MP3/AAC at 48 kHz stereo is a few percent of one core.
  Hi-res (96/192 kHz) is resampled to 48 kHz by the existing resampler (the TV's PCM input stops at 48 kHz).
- Loss: surround music and bit-perfect hi-res output. Acceptable for a TV; stated in the settings text.

The engine, in pieces:
1. `TrackSource`: HTTP range reads of the Part (direct) or the universal transcode URL, demuxed and
   decoded to PCM by the bundled FFmpeg (confirm the AAC/MP3/FLAC/ALAC/Opus/Vorbis decoders are in the
   vendored build: M0).
2. `Gapless queue`: while track N plays, track N+1's first ~15 s are fetched, demuxed and decoded into a
   ring; at the boundary the ring continues into the next track with no silence. Needs both tracks' PCM
   to be 48 kHz S16 stereo, which the converter already guarantees.
3. `Mixer`: sums the outgoing track's tail and the incoming head for crossfade (0 s = pure gapless),
   applies gain (ReplayGain-style from Plex's loudness, album mode when the album plays in order),
   applies the fade for pause/stop/sleep.
4. `Tap`: a lock-free copy of the mixed samples for the visualiser (the UI thread does a 1024-point FFT).
5. `Sink`: one `Load` per session, audio only, PCM, and then a steady feed. Seek = flush and refeed from
   the new offset (the existing seek machinery is a reference, not shared code).

### 3.3 The unknowns, to be settled first (M0, a spike with no UI)
- **S1** Does the pipeline on webOS 4.10 (OLED) and the SM8600 accept an audio-only PCM `Load`? If it
  refuses, fall back to feeding a one-frame black H.264 stream alongside the audio.
- **S2** Which FFmpeg decoders are in the vendored build and do they keep up on both TVs?
- **S3** Does music survive the Home key (Blade Home / `go_home`)? Today `quit` ends the app. If webOS
  suspends a background native app we need the audio to stop cleanly; if it lets it run, we get background
  play for free. This decides Q1.
- **S4** Sink buffering for a steady feed: underrun behaviour on Wi-Fi (the 8600 drops packets).
- **S5** The exact universal-transcode query for audio, and what the server does with a transcode when a
  sample rate or bit depth is outside our table.

A go/no-go on S1 and S2 gates everything else.

### 3.4 Interaction with video
One pipeline at a time. Starting a film or channel stops the music (state saved: the queue stays so
"Resume music" is one press). Starting music while a video plays is not offered. Live TV likewise.

## 4. Playback model

- **Queue**: the server's play queue is the source of truth (`playQueueID`, items with
  `playQueueItemID`); a local mirror keeps the screen fast. Operations: Play now (replaces), Play next,
  Add to queue, Shuffle (server shuffle, keeps current), Repeat (off / all / one), Clear, Reorder, Remove.
- **State machine** (`Idle -> Loading -> Playing <-> Paused -> Ended/Error`), exactly one `NowPlaying`
  descriptor drives the mini player, Now Playing and lock-style surfaces.
- **Reporting**: timeline every 10 s (`ratingKey`, `key`, `playQueueItemID`, `state`, `time`, `duration`),
  scrobble comes from the server's own 90% rule via the timeline.
- **Errors**: a track that fails is skipped with a toast after one retry, queue continues; three failures
  in a row stop with a message.
- **Persistence**: the last queue, position and shuffle/repeat per profile (small JSON beside
  `layout.json`) so "Resume" works after a reboot.

## 5. Information architecture

The sidebar gains one entry per music library (named as the server names it, with a note icon).
`SecKind` gets `Artist`; the compiler will then force a decision at every exhaustive match
(`kind_offers_plays_sort` is the known one). `PmsMovie.kind` gets `5 artist`, `6 album`, `7 track`,
`8 playlist`; `pms::listable` and every `match item.kind` site is audited in M1 (a checklist item, not a
guess: grep `kind ==` and `.kind {`).

```
Sidebar
 ├ Search                         (artist / album / track shelves added)
 ├ Home                           (music rows join, see 6.1)
 ├ Music (library)  ── tabs: Recommended · Artists · Albums · Tracks · Playlists · Collections
 │     ├ Artist page ─ Album page ─ (track menu)
 │     └ Playlist page
 ├ …other libraries, Live TV, Watchlist
Global while music plays
 ├ Mini player  (every page except the video player)
 └ Now Playing ─ Lyrics · Queue · Track info · More
Settings ─ Music
```

## 6. Screens

All sizes are on Amber's 1920x1080 logical canvas. Mock-ups for each are in the companion page.

### 6.1 Music Home ("Recommended" tab) and the main Home
Same layout as a Home page: details panel for the focused item on top (title, artist, album/year, runtime,
a Lossless/Hi-Res chip), shelves beneath. Shelves (server hubs, titled by the server): Recently Played,
Recently Added, Most Played, Rediscover, Random Albums, Mixes, Playlists. Cards are **square** (album cover
250x250), artists are **circles**, playlists are a 2x2 composite (the baked fan used for collections).
The main Home page lists *Recently Added Music* and *Recently Played* after the TV rows, hideable and
re-orderable in the existing Home editor (`home_order` ids `home.music.*`).
Focus on an album card: OK opens the album page, Play key plays it, long-press opens the item menu.

### 6.2 Library tabs: Artists, Albums, Tracks, Playlists, Collections
- **Artists**: 6-column grid of circular portraits, name under, "12 albums" sub-line.
- **Albums**: 6-column grid of square covers; title, "Artist · 2019" under.
- **Tracks**: a table (index in the list, title, artist, album, duration, rating heart), A-Z rail, the
  Library filter row (`All` / genre / decade / `by` sort). Play and Shuffle buttons at the right of the
  filter row, as the official app has.
- **Playlists**: grid of composite tiles; smart playlists carry a small spark badge.
- Filters/sorts come from the server per section (`includeMeta=1`), exactly as movies do today.

### 6.3 Artist page
Backdrop = artist art (or blurred portrait), left column over a dark fade: name (headline), genre chips,
the first lines of the bio (OK opens the full bio panel, reusing the Person page's `person_bio`), buttons:
**Play · Shuffle · Radio · More**. Then rows: **Popular** (5 track rows with a play count), **Albums**
(square covers, newest first, grouped under Albums / Singles & EPs / Live / Compilations when the server
types them), **Appears on**, **Similar artists** (circles). Back returns to where you came from.

### 6.4 Album page
Left: large square cover (520x520) with soft shadow. Right: title, artist (OK goes to the artist), year ·
genre · label · total time, a Lossless chip, buttons **Play · Shuffle · Add to queue · Mix · More**, a
rating heart. Below: the **track list** (number, title, artist when it differs from the album artist,
duration, heart, an animated equaliser glyph on the playing row). Multi-disc albums get "Disc 1", "Disc 2"
headers. Then **Review** (when the server has one), **More by this artist**, **Similar albums**.

### 6.5 Playlist page
The album layout with a composite cover, the owner and "n tracks · 3h 12m", Play/Shuffle, track list with
the source album art as a small thumbnail per row. Edit mode (v2): colour-key reorder (the same pick-up and
place gesture used in the Live TV guide), remove, rename. Delete only from the menu with a confirm.

### 6.6 Now Playing (full screen)
- Page wash from the artwork's four-corner colours (the machinery Home uses), plus an optional visualiser
  layer (6.9). OLED-safe: no static bright element longer than the screensaver delay (see 6.10).
- Left: cover 640x640 at (160,170). Right column from x=900: title (56 px), "Artist · Album" (28 px),
  a chip row (Lossless 24-bit/96 kHz, year), the **progress bar** (880 px, elapsed and remaining under
  it; with `/levels` the bar can show the loudness shape, v2), the **transport**: Shuffle, Previous,
  **Play/Pause** (large), Next, Repeat; a second row: **Heart, Lyrics, Queue, More**.
- A quiet **Up next** peek (next three tracks, small type) bottom right; Down opens the full queue.
- Back returns to the page you opened it from; the music keeps playing.

### 6.7 Lyrics
Cover shrinks to 360x360 on the left. The right pane shows lyrics; synced lyrics scroll with the current
line large and bright, neighbours dim; unsynced lyrics scroll by hand (Up/Down). A track with none shows
"No lyrics" and the key is greyed. Plex Pass only; the toggle is hidden if the server does not offer them.

### 6.8 Queue (Up next)
A right-hand glass panel (reusing the player panels' modal machinery): "Now playing" row, then "Next from
album", then "Up next", each a track row. Colour key **pick up**, Up/Down to place, OK to drop (the Live TV
gesture). Row menu: Play next, Remove, Go to album/artist. Footer: **Clear**, **Save as playlist**.

### 6.9 Visualiser
Optional layer behind Now Playing: spectrum bars or a soft waveform ring in the artwork's accent colour,
driven by the PCM tap. Draws at 30 fps into the existing wash layer; off by default on the SM8600 until
measured. Settings: Off / Bars / Wave.

### 6.10 Ambient Now Playing and the screensaver
After the screensaver delay, a music-aware saver replaces the generic one: the cover drifts slowly at low
brightness with title/artist and the clock, repositioned every minute (OLED burn-in safe). It never
starts while a lyric line is changing under input. A setting turns the picture off entirely after N
minutes while the music plays (`turnOffScreen`, *verify permission*, else the dim saver only).

### 6.11 Mini player
A glass capsule, bottom of every page except the video player, 560x80, offset from the sidebar edge:
36 px cover, title, artist, a thin progress line, a play/pause glyph. It overlays content without
reserving layout; it auto-hides after 6 s of idle on pages with a bottom shelf and returns on any track
change, and shows a **blue** key hint. Always reachable: press **blue** anywhere to open Now Playing;
OK on the capsule when focused (Down from the bottom row of a page) also opens it.

### 6.12 Search
Search gets three more shelves: Artists (circles), Albums, Tracks (rows). Results keep the existing
ranking; track rows play immediately (OK) and offer the item menu (hold).

### 6.13 Item menu and Add to playlist
The item menu (`screens/item_menu`) learns music kinds. Track: Play, Play next, Add to queue, Start radio,
Go to album, Go to artist, Add to playlist, Heart, Track information. Album: Play, Shuffle, Play next,
Add to queue, Add to playlist, Go to artist, Mix. Artist: Play, Shuffle, Radio, Go to artist. **Add to
playlist** is a small panel listing audio playlists plus "New playlist" (a text entry via the existing
on-screen keyboard from Search).

### 6.14 Settings > Music
- Playback: Gapless (on), Crossfade (Off, 2, 4, 6, 8, 12 s), Loudness levelling (Off / Track / Album).
- Quality: Original (default) / High (transcode ~320 kbps) / Medium; "Convert on slow networks".
- Display: Visualiser (Off / Bars / Wave), Lyrics (Auto / Off), Now Playing screensaver behaviour.
- Library: show Rating as heart / stars; sleep timer default.

### 6.15 States common to all
Loading skeletons (square/circle), empty library ("No music yet"), failed fetch (retry), an unplayable
track (greyed with the reason), offline server, a library the profile cannot see. Same read-outs as the
video pages (`StatusOverlay`).

## 7. Remote keys

| Key | Anywhere (music playing) | Now Playing |
|---|---|---|
| Play/Pause (media key) | toggle | toggle |
| Stop | stop and clear the mini player | same |
| FF / REW | next / previous track | seek +/- 10 s (hold 30 s) |
| CH + / CH - | next / previous track | next / previous track |
| Blue | open Now Playing | close |
| Red / Green / Yellow | page-specific | Shuffle / Repeat / Lyrics |
| Info | track information | track information |
| Back | page back | back to the page |
| OK on a track row | play from here (album/playlist = whole list queued) | |

The hint line shows the coloured key legends, as Live TV does. Volume stays the TV's.

## 8. Code architecture (Amber's layers, `docs/module-layers.md`)

| Layer | New things |
|---|---|
| `plex` | `plex::music` client: artists/albums/tracks listing, popular leaves, nearest, lyrics, levels, playlists, play queues, rate; DTOs for `Metadata` kinds `artist/album/track/playlist` and the audio stream's gain/peak |
| `data` | `SecKind::Artist`; `PmsMovie.kind` 5-8; `stores::music` (artist/album/playlist detail, like `metadata`); queue store; hubs for music sections; search shelves; layout ids `home.music.*` |
| `media` | `MusicEngine` (3.2): `track_source`, `gapless`, `mixer`, `tap`; reuse `play-feed`'s sink and PCM decoder; no `PlaybackSession` use |
| `ui` | `Art::Square` and `Art::Circle` card variants, a track-row widget, an equaliser glyph, a star/heart rating, an artwork-tinted progress bar, a spectrum widget; image sizes: square card 300x300, Now Playing 800x800, backdrop 1600x900 |
| `appkit` | mini-player state, now-playing descriptor |
| `screens` | `music::{home, artists, albums, tracks, artist, album, playlist, now_playing, lyrics, queue}`; item menu additions; settings page |
| `app` | wiring, key routing for global media keys, resume at boot, screensaver mode |

New screens are `Screen`/`Machine`/`Focusable` implementations like the existing ones, with the same
test pattern (host tests with fixtures; no Plex server needed).

## 9. Budgets and risks

- Memory: square art is small (300x300 = 0.36 MB); Now Playing art 800x800 = 2.5 MB; the decoded PCM ring
  for gapless ~15 s x 48 kHz x 4 B = 2.9 MB per track. Total add well under 20 MB.
- CPU: decode + resample + mix + FFT must stay under ~10% of one core on the 8600; M0 measures it.
- The UI must not hitch audio: the engine runs on its own worker with a bounded ring; UI work never
  blocks the feed.
- Wi-Fi: the 8600 drops packets; the prefetch depth (15 s) doubles as the network buffer, adjustable.
- Risk: S1 fails and the dummy-video fallback misbehaves. Mitigation: it is the first thing tested.
- Risk: a large library (50k tracks) in the Tracks tab. Mitigation: paged listing as movies already do.
- Localisation: ~80 new strings; English first, the existing machine-translation flow for the rest.

## 10. Milestones

| | Delivers | Exit test |
|---|---|---|
| **M0 spike** | Command-line style test build: play one FLAC, one MP3, one AAC from the server on both TVs, gapless pair, 30 min soak, no UI | S1-S5 answered; CPU/underrun numbers |
| **M1 playback core** | `MusicEngine`, queue store, timeline, music kinds in data, album page + Now Playing (minimal), reachable from a deep link / dev trigger | an album plays gapless, reports to Plex, survives seek and skip |
| **M2 browse** | Music sidebar entry, Recommended, Artists, Albums, Tracks, artist page, album page, Home rows | browse and play from a cold start with the remote only |
| **M3 shell** | Mini player, global keys, queue screen, resume at boot, screensaver mode, search shelves, item menu | music keeps playing across pages; no regressions in video |
| **M4 depth** | Playlists (view, add, create), rating heart, lyrics, radio via `nearest` | |
| **M5 polish** | Crossfade, loudness levelling, visualiser, Settings > Music, waveform scrubber | measured on both TVs |

Each milestone ends installed on both TVs for you to try, as with everything else.

## 11. Open questions for you

1. **Q1 Background play.** Should music keep playing after you press Home (Blade Home) and open another
   TV app? It depends on S3; if webOS allows it I would add it, and the mini player then lives only in Amber.
2. **Q2 Library.** Do you have a music library on the server today, and is Plex Pass active (lyrics and
   sonic radio need it)? What is in it: mostly albums as FLAC, MP3, or hi-res?
3. **Q3 Rating.** Heart only (rating 10 / unset), or full stars as well?
4. **Q4 Visualiser** on or off by default?
5. **Q5 Screen-off.** OK for Amber to turn the picture off while only music plays (OLED friendly), if the
   TV lets an app do it?
6. **Q6 Scope.** Is everything in section 6 wanted, or should playlists editing / radio / lyrics wait?

## 12. M0 result (2026-11-27, OLED65B9PTA, webOS 4.10.2)

- **S1 answered.** The Starfish pipeline accepts an audio-only PCM Load and takes every sample, but plays
  **nothing** while no picture has been decoded: the output is only bound to the TV (the ACB plane bind that
  names the media id and sets PLAYING) once the pipeline has a video frame to describe. An audio-only Load
  never produces the media id.
- **What works:** declare H264 beside the PCM audio, feed one black 320x180 H.264 picture (830 bytes, Annex B)
  once a second, bind on the media id the pipeline then reports, send the pipeline its own source info, start
  the plane, and feed 48 kHz S16 stereo PCM. The tone was heard. Feed stayed 1.5 s ahead with no buffer-full
  replies and no errors for 20 s.
- **Consequence for the design:** the music engine is the "dummy video" fallback (3.3 S1), not a pure audio
  pipeline. The black picture sits behind the UI, which is drawn opaque over it on every music screen.
- **Still open:** S2 (decoders), S3 (background after Home), S4 (long soak and Wi-Fi), S5 (transcode query),
  S6 (what the Gen 1 Beam receives), and the same test on the SM8600.
- Decisions recorded: background play wanted; ratings are stars; visualiser off by default; a key turns the
  picture off while music plays; stage 1 = everything except playlist editing and lyrics.
