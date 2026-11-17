# Amber

A fork of [PlxNative](https://github.com/plxnative/plx-native) (GPL-3.0-or-later) that keeps its native
speed and gives it the look and feel of the official Plex app on LG webOS. Not affiliated with Plex or
with PlxNative. Everything upstream documents still applies unless this file says otherwise.

## What is different from upstream

| | upstream | Amber |
|---|---|---|
| app id / name | `com.beb.plxnative` / PlxNative | `com.dgaust.amber` / Amber (installs beside PlxNative) |
| artwork | PLX logo and splash (reserved to upstream) | its own amber chevron mark |
| accent | white material tinted by the page | Plex amber (`theme::ACCENT`, `CONTROL_FOCUS_TOP/BODY`, `CARD_FOCUS_RING`) |
| focused card | scale and glow | scale, glow and an amber outline (`widgets::card_named_led`) |
| navigation | a pill bar across the top | a sidebar down the left edge (`widgets::rail_cell`, `StripRender::draw`) |

The sidebar is the SAME strip as upstream's top bar - same keys, same members, same focus group
(`containers::tabs::STRIP`). Only three things changed: where the members stand (`tab_geometry` puts
them in a column of `RAIL_CELL` squares under the profile chip), how they are drawn (marks in the
gutter at rest; a dark panel with their names while focus is in the bar), and which keys travel it
(the group is a `Column` reached with LEFT; UP still reaches it from the top of a page through the
pages' own links). It no longer lifts off the top when a page scrolls (`screen::chrome_lift` is 0).

## Building

- **The TV package** is built by GitHub Actions: `.github/workflows/amber-build.yml`, on every push to
  `amber` and on demand (`gh workflow run amber-build.yml -f flavor=stable|debug`). The ARM build
  needs an arm64 Linux or macOS host - the webOS NDK has no x86-64 Linux build - so it cannot be
  built on a Windows PC. The `.ipk` is the run's artifact (`gh run download <id>`).
- **The simulator** builds and runs on x86-64 Linux, including WSL: `make sim-linux`
  (about 3 minutes after an edit). It runs headless under `xvfb-run`.
- Upstream's own release, nightly and site workflows are still in the tree and are not used.

## Looking at a change without a television

`tools/screenshots.py` renders named scenes from `tests/screenshots/scenes.json` against the mock
server's demo library (no Plex account). Two additions here:

- the website-only outputs were dropped from the scene list (they need a headless browser);
- a scene may press remote keys: `"remote": [[seconds_after_boot, "left"], [4.2, "ok"]]`, written to
  the app's key FIFO, with an `after_ms` later than the last press. The `amber-rail-*` scenes use it.

```sh
xvfb-run -a -s "-screen 0 1920x1080x24" python3 tools/screenshots.py \
  --bin rust-modules/target-sim/release/plxnative-sim --out /tmp/shots --only library,amber-rail-lib
```

`python3 tools/demo_library.py derive` fetches the demo artwork once (needs Pillow and ffmpeg).

## Since the first cut (2026-11-17/18)

- **The sidebar is on every page but the player** - Home, the libraries, Search, Watchlist, Live TV
  and the item pages (movie, show, season, episode, person, collection: `AppArg::Content` wears
  `Chrome::TabBar`). Left from a page's leftmost stop reaches it (item-page groups are reachable
  only vertically, so a sideways search lands on nothing but the sidebar); an item page marks the
  entry it was opened from. Entries can be reordered and hidden (*Edit sidebar*), and Home's rows
  moved and hidden from a card's menu (`data::layout`, saved beside the app's state).
- **The page is shifted right of the sidebar** by `widgets::page_shift`; anything a page places by
  screen width (the A-Z and decade rails, the library header) is laid out in page space, the
  screen's width less `PAGE_SHIFT_CLOSED`.
- **Home's billboard** shows Continue Watching's art only (topped up to five from the rest of the
  catalog, rated M or under, when Continue Watching has fewer than three). It has no buttons or
  pager and is not a focus stop: Home opens on the first row's first card under it.
- **Live TV** (`plex::livetv`, `screens::livetv`, `app::livetv`): the DVR's channels and a guide
  grid (now line, aired part dimmed, details of the focused programme). OK on an airing on now
  tunes it - by the tuner's channel from the DVR's mapping; PMS 1.43.5 refuses the guide's own id
  with 400 - and plays `/livetv/sessions/<uuid>` through the ordinary player path.
- **A clock** top right on every page with the sidebar.

## Not done

- **Live TV playback is not yet confirmed on a television**: the guide loads from the real server;
  the tune by the tuner's channel is the build after `e12b00ea`.
- Live TV's *What's On* rows, the live player overlay (channel up/down, next programme) and Plex's
  free channels.
- The library page's filter row, item count and Play / Shuffle; the detail page's round icon
  buttons. See `docs/amber-screen-map.md`.
- Upstream tests that pin the top bar or the billboard as a focus stop are `#[ignore]`d with the
  reason; `cargo test -p plx_screens -p plx_plex -p plx_data` passes.
- Music, photos and playlists are not in upstream either.
