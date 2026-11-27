# Amber Music: build status

Branch `music` (github.com/dgaust/amber-tv). `amber` is untouched and still the stable build for the TVs.
Design: `docs/music-design.md` (decisions, screens, M0 result). This file says what is built and what is not.

**Nothing here has been heard on a television yet** apart from the M0 tone test. The engine and every page
compile and have unit tests for their pure parts, but playing a real track, browsing against your server and
the layout of each page are untried. Expect to find and fix things on the first run.

## Built

- **Plex client** (`plex/src/plex/music.rs`): music sections, artist/album/track/playlist listings (paged), an
  item's children, an artist's tracks, popular tracks, hubs, search, sonic "nearest", rating, timeline reports,
  the audio file fetch. Tested against sample JSON.
- **Decoder** (`play-feed/src/music_decode.rs`): a downloaded file read through a memory AVIO and decoded by the
  bundled FFmpeg to 48 kHz stereo PCM on a worker thread, any codec the build has. A bounded queue keeps memory to
  a few seconds of PCM.
- **Engine** (`src/app/music.rs`): one pipeline Load for a whole listening session; tracks follow each other on
  one continuous feed (gapless); the next track's file is fetched while the current one plays; a black frame once
  a second so the TV binds its output (the M0 finding); play, pause, next, previous, seek, jump, stop; shuffle and
  repeat; crossfade and loudness levelling (settings, both off by default); timeline reports to Plex every 10 s;
  release of the pipeline on stop and before any film or channel starts.
- **Music page** (`screens/src/music.rs`), a sidebar entry "Music" when the server has a music library:
  Recommended (the library's shelves), Artists, Albums, Tracks, Playlists, Settings; an artist page, an album or
  playlist page, Now Playing (progress with seek, shuffle / previous / play-pause / next / repeat, half-star
  rating, queue, radio, screen off, stop), the queue. A strip along the bottom while music plays on other music pages.
- **Keys**: play/pause, stop, fast-forward/rewind and CH+/CH- work from any page except the video player while music
  plays. On the music page, yellow blacks the picture out (any key brings it back), blue opens Now Playing, a held OK
  on a track plays it next.
- **Simulator**: `MOCK_MUSIC=1` makes `tests/mock_pms.py` serve a small music library (12 artists, 3 albums each).
  The simulator itself was not run (its build did not finish).

## Not built

- Search for music, playlist editing, lyrics (stage 2, as agreed).
- The mini player on pages outside the Music page (media keys work everywhere; only the on-screen strip is missing).
- The visualiser, and turning the TV's own panel off (the black-out is drawn by Amber; an LS2 `turnOffScreen` call
  was not added).
- Dolby Digital / DTS passthrough for surround tracks, background play after Home, the music screensaver mode.
- Music rows on the main Home page.

## Where to look first when something is wrong

- Event log on the TV, `music:` lines (`/tmp/com.dgaust.amber/plxnative-events.log`): "starting the pipeline",
  "pipeline loaded", "output bound", "output started", "playing", "decoding ...", "decode failed: ...".
- Silent but "playing" in the log: the output bind (needs the media id the pipeline reports after the first black
  frame) did not complete.
- A track that will not play: "could not fetch the file" (the part key or size) or "decode failed" (a codec the
  bundled FFmpeg lacks: the log names it).
- Positions drift or the next track starts late: the position clock is software, offset 400 ms for the PCM lead.
