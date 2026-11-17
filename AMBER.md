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

## Not done

- **Not yet run on a television.** Everything above was verified in the simulator only.
- **Upstream's host test suite has not been run** against these changes; its geometry and legibility
  tests for the top bar will fail by design and need rewriting for the sidebar.
- **Live TV and the guide.** Upstream has no Live TV at all (it is out of its scope), so this is a new
  data layer, screens and a playback path, not a restyle. Needs a real server to develop against.
- **Home and detail layouts** are still upstream's (hero carousel; backdrop detail page).
- Music, photos and playlists are not in upstream either.
