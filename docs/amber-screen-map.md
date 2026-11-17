# Amber screen map — the official webOS Plex app, screen by screen

Mapped on 2026-11-17 by driving the official Plex app (LG app id `cdp-30`) on an LG SM8600 with
injected remote keys and capturing each screen. The captures show a real household's library and
account, so they are kept off this public repository; this file records the STRUCTURE.

Status column: **have** = Amber already does it; **partly** = present but different; **missing**.

## 1. The shell (every page except the player)

| Element | Official app | Amber |
|---|---|---|
| Left sidebar, closed | a column of faint line icons at the left edge, the page beside it | **have** |
| Left sidebar, open | ~400px dark panel: avatar + profile name at the top, then Search, Home, each library by name (TV Shows, Movies, ...), Watchlist, **Live TV**, then the other libraries (Music, Photos, ...). The page is pushed right, never covered | **have** |
| Sidebar focus | a **white plate** with dark text and icon | **have** (white plate) |
| Moving in the sidebar | moving the highlight switches the page behind it at once; OK/Right goes into the page | **missing**: Amber needs OK |
| Back | from a page, Back returns focus to the sidebar on that page's entry | partly |
| Section tabs | a row of text tabs at the top of the page, selected one with an amber underline: Home: *Home, Trending, Activity, Find Friends, Profile*; a library: *Recommended, Library, Collections, Categories*; Live TV: *Guide, What's On • server, DVR, Browse* | **missing** (Amber has a library chip row) |
| Clock | top right on every page | **have** |

## 2. Home

- No hero carousel. The top ~40% is a **details panel for whatever is focused**: title, a second
  line (episode title / date), a metadata row (S·E, date, runtime, rating, a "42 min left" chip),
  a 3-line synopsis, and a cast/creator line. The focused item's backdrop fills the right half,
  fading into the page on the left.
- Below it, **rows of portrait posters** (2:3), titled: *Continue Watching*, *Recently Added TV*,
  *Recently Added Movies*, then server/collection hubs. One row per kind across all libraries.
- Focused poster: a **white outline**, no scale. Progress bar along a poster's bottom (amber).
  Watched: a check in a dark corner square; unwatched episode count in the same square.
- Moving down scrolls rows up under the details panel; the panel updates with focus.

## 3. A library

- **Recommended** tab: Home's layout (details panel + rows), the library's own hubs
  (*Continue Watching*, *Recently Released Movies*, ...).
- **Library** tab: a filter row - `All ▾` `TV Shows ▾` `by` `Title ▾` - the item count
  ("413 Shows"), and Play and Shuffle buttons at the right; an **A–Z rail** down the right edge;
  a 6-column poster grid, each poster captioned with its title and a sub-line
  ("8 Episodes", "7 Seasons", "2026 · 1h 35m").
- **Collections** and **Categories** tabs: grids of collections / genres.

## 4. Detail (a show or film)

- Backdrop art on the right two-thirds; content on the left over a dark fade.
- Title (large), metadata row (year, runtime, rating, a *✓ Watched* chip), genres,
  ratings (critic/audience) and a *Rate & Review* pill, synopsis (3 lines).
- A row of **round icon buttons**: Play (a white pill labelled *Play* when focused), Watchlist,
  Trailer, Mark watched, Share, More (⋯).
- *Activity by You* chip.
- **Episodes** row of 16:9 stills captioned "Episode title" / "S1 • E1 - Episode title", with
  watched checks; then (scrolling) Seasons, Cast & Crew, Related.

## 5. Live TV

**Guide** tab (the default):
- The details panel for the focused programme (title, episode, S·E, runtime, rating, a
  "6m left" chip, synopsis).
- A source row: *HIPOSERVER* (the household's DVR) | *Plex Channels* | categories
  (Featured, Bingeworthy, Movies, True Crime, News, Sports, Reality, Classics, ...).
- The **grid**: a time ruler (*TODAY* | 7:30PM | 8:00PM | 8:30PM | 9:00PM - half-hour
  columns); one row per channel with its logo tile and channel number on the left; programme
  cells with a thumbnail, title and "7:39PM - 8:01PM". The part of a programme already aired is
  dimmed; the focused cell is a white plate reading "Title / 6 min left".
- OK on a programme on now tunes the channel and plays it full screen.

**What's On** tab: rows - *Recent Channels* (channel-logo cards with a red LIVE badge, the
programme name and "5 min left", an amber progress bar), *What's On Now* (programme posters with
LIVE and progress), *Shows On Now*, ...

**Live player** (OK on a channel): bottom overlay - programme title, "Episode • 7:42 PM - 8:03 PM",
the next programme at the right ("The Simpsons • 8:03 PM"), a full-width progress bar with the
live position, the programme's start and end clock times under its ends, an *HD* badge,
*♡ Channel* (favourite), previous-channel / pause / next-channel, and ⋯ More.

**The household's server** (from Amber's own probe, `livetv:` in its log): one DVR,
lineup "Local Streams", EPG `tv.plex.providers.epg.xmltv:18`, two tuner devices.
The Plex free channels ("Plex Channels") come from plex.tv, not the server.

## 6. The player (films and episodes)

Not yet captured; Amber's player HUD is upstream's.

## Build order

1. Shell: white sidebar focus, profile row, Search first, Live TV entry, move-to-switch,
   clock, per-section tab row.
2. Home: replace the hero carousel with the focus-driven details panel; portrait rows,
   white outline focus.
3. Live TV: Guide grid against the server's DVR (channels, XMLTV guide, tune and play),
   then What's On and the live player overlay.
4. Library tab: filter row, count, Play/Shuffle, A–Z rail, captions.
5. Detail: left-aligned details with round icon buttons, Episodes row.
6. Plex Channels (plex.tv free live TV), DVR recordings tab.
