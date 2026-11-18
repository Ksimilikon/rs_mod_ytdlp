# rs_mod_ytdlp

A dream_player mod: searches YouTube and SoundCloud, lets you listen before
saving, and downloads audio from YouTube, SoundCloud (and other sites
[yt-dlp](https://github.com/yt-dlp/yt-dlp) supports) into the music folder
and adds it to the library.

## What it needs

- the core built with `--features modding` and having `core_get_dir`;
- a `yt-dlp` executable (from `PATH` or a path set on the tab);
- `ffmpeg` only for mp3 / vorbis / flac and for embedding tags / cover.
  The default format takes the site's own stream as is — AAC (m4a) on
  YouTube, mp3 on SoundCloud / Bandcamp: no ffmpeg needed.

## The tab ("YouTube")

- link field: one or more links separated by spaces; **Download** queues
  them, **Cancel** kills the running yt-dlp and drops the queue;
- **Link cleanup** toggles, each independent (`src/clean.rs`):
  - remove playlist position — `list`, `index`, `start_radio`, `playnext`
    (also passes `--no-playlist`; a pure playlist link is rejected);
  - remove share tracking — `si`, `feature`, `pp`, `utm_*`, `fbclid`, ...;
  - remove start time — `t`, `start`, `end`, `#t=`;
  - remove any other parameters;
  - rewrite to `https://www.youtube.com/watch?v=ID` (shorts, youtu.be,
    music.youtube.com, embed, live, m.youtube.com).

  Non-YouTube links pass through untouched.
- **Download options** (toggles and a format select): format (original
  m4a/mp3 / mp3 / vorbis / flac — only what the player can decode), embed
  metadata, embed cover, add to the library;
  yt-dlp / ffmpeg paths, subfolder in the music folder (default `YouTube`),
  extra yt-dlp arguments (e.g. `--cookies cookies.txt`);
- **Search YouTube / SoundCloud**: "Search on" (YouTube or SoundCloud —
  yt-dlp's `ytsearchN:` / `scsearchN:`), a query field, **Search**,
  **Stop listening**, **Save to library**, and the results list —
  `[duration] title — channel / uploader` (`src/search.rs`,
  `src/browse.rs`; the count — 5 / 10 / 20 / 30 — is the "Search results"
  option). SoundCloud gives AAC (m4a) over HLS: the default format needs no
  ffmpeg there either; Go+ tracks may come as 30 s snippets without an
  account (`--cookies` in the extra arguments):
  - picking a result downloads its audio (only audio, in the chosen format)
    into `<cache>/rs_mod_ytdlp/preview` and plays it through the player;
    picking it again pauses / resumes; picking another one cancels the
    current download;
  - **Save to library** moves the listened file into the music folder (and
    the library, if enabled) — no second download. Pressed while the file is
    still loading, it saves it when done; on a result never picked, it
    queues a plain download;
  - marks in front of a result: `…` loading, `▶` playing, `✓` saved,
    `⇣` queued, `✗` failed;
  - previews not saved are deleted on a new search and when the mod stops.
  Searches and previews run on their own thread, never behind the download
  queue.
- **Recent**: results, newest first. An `Info` element carries the paths of
  the downloaded files for the frontend (e.g. to play them).

Settings are saved to `<config>/mods/rs_mod_ytdlp.conf`; partial downloads
go to `<cache>/rs_mod_ytdlp`, never to the music folder.

## Platforms

| platform | how the mod gets in | yt-dlp |
|---|---|---|
| linux / macos / windows | `cargo build --release` (in this dir; expects `../rs_dream_player` next to it), copy the `.so` / `.dylib` / `.dll` to `<data>/mods` | from `PATH` or set path |
| android | link the crate (rlib) into the frontend, `mods_add_static("ytdlp", &rs_mod_ytdlp::MOD)` | the frontend ships a runnable yt-dlp (e.g. a python build) and sets its path |
| ios | statically, like android | not possible: iOS forbids spawning processes; the mod reports it |

## Tests

```
cargo test                 # unit + e2e in a real core (offline, local http)
cargo test -- --ignored    # real YouTube download, YouTube / SoundCloud search (network)
```
