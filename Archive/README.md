# Ytermusic

Ytermusic is a keyboard-first YouTube Music player for the terminal, written in
Rust. It searches and streams music, browses country-specific trending charts,
resumes podcasts, manages a sequential or shuffled queue, runs endless radio,
and smooths playback transitions with configurable fade-in and fade-out.

## Anonymous quick start

An account is optional. After installing the playback dependencies and
Ytermusic, check the environment and launch the TUI:

```text
ytermusic doctor
ytermusic
```

For a fresh session:

1. Press `/` to open search.
2. Type a query and press `Enter` to submit the query.
3. Wait for results, then select a result with the arrow keys or `j`/`k`.
4. Press `Enter` again to replace the queue and play the selected result.

`Space` pauses or resumes an already-active item; it cannot enqueue or start a
fresh search result.
Anonymous browsing, search, charts, podcasts, queues, and playback work without
importing a browser session. From a source checkout, install the executable with
Rust 1.97 or newer:

```text
cargo install --locked --path .
```

## Install

Ytermusic requires `mpv` for audio output, `yt-dlp` for resolving stream URLs,
and `ffmpeg` for the media formats used by those tools. Current `yt-dlp`
releases also use an external JavaScript runtime to solve YouTube JavaScript
challenges; Ytermusic installs the recommended `deno` runtime for that purpose.
Install all four before starting Ytermusic.

### macOS

Install the dependencies with Homebrew:

```text
brew install mpv yt-dlp ffmpeg deno
```

Then install Ytermusic from a source checkout with
`cargo install --locked --path .`. The template in
`packaging/homebrew/ytermusic.rb` is for release automation and must not be
published until its URL and checksum placeholders have been replaced.

#### Makefile workflow

From a source checkout, `make build` (an alias for `make build-debug`) creates a
native debug build, while `make run` runs the native debug executable.
`make build-release` creates an optimized native build. Use `make check` to run
formatting, Clippy, and tests.

On macOS, `make package` combines optimized ARM64 and Intel builds into a
universal macOS archive under `dist/`. To run the complete local preparation
workflow, use `make release-local GITHUB_OWNER=your-github-name`; the owner is
used to render the generated Homebrew formula's GitHub URLs. This prepares
local artifacts under `dist/`. It does not create Git tags or upload GitHub
releases; tagging and upload remain manual.

### Linux

Use the package manager for your distribution. For example:

```text
# Debian or Ubuntu
sudo apt update
sudo apt install mpv yt-dlp ffmpeg

# Fedora (enable RPM Fusion if your release does not provide ffmpeg)
sudo dnf install mpv yt-dlp ffmpeg

# Arch Linux
sudo pacman -S mpv yt-dlp ffmpeg

# Deno's official installer (all Linux distributions)
curl -fsSL https://deno.land/install.sh | sh
```

Then install Ytermusic from a source checkout with
`cargo install --locked --path .`. Distribution package names can vary. The
Deno installer normally adds `~/.deno/bin` to `PATH`; open a new shell if
`deno --version` is not initially found.

### Windows

In PowerShell, install each dependency with WinGet:

```text
winget install --exact --id shinchiro.mpv
winget install --exact --id yt-dlp.yt-dlp
winget install --exact --id Gyan.FFmpeg
winget install --exact --id DenoLand.Deno
```

With Scoop, add the non-default `extras` bucket before installing `mpv`, then
install the dependencies available from the default `main` bucket:

```text
scoop bucket add extras
scoop install extras/mpv
scoop install main/yt-dlp main/ffmpeg main/deno
```

Then install Ytermusic from a source checkout with:

```text
cargo install --locked --path .
```

The Scoop and WinGet files under `packaging/` are release templates. Release
automation must replace their URL and checksum placeholders before publication.

## Keyboard reference

The same bindings appear in the in-app help and command palette. `Ctrl-C` is a
global quit shortcut in addition to `q`.

| Key | Action |
| --- | --- |
| `q` | Quit |
| `?` | Toggle help |
| `/` | Open search |
| `:` | Open command palette |
| `Space / F8 / Media Play/Pause` | Play or pause |
| `n / F9 / Media Next` | Next track |
| `p / F7 / Media Previous` | Previous track |
| `Shift+Left` | Seek backward |
| `Shift+Right` | Seek forward |
| `↑ / k` | Move up |
| `↓ / j` | Move down |
| `← / h` | Move left |
| `→ / l` | Move right |
| `Tab` | Focus next region |
| `Shift+Tab` | Focus previous region |
| `+` | Volume up |
| `-` | Volume down |
| `s` | Toggle shuffle |
| `r` | Cycle repeat |
| `e` | Toggle endless radio |
| `f` | Toggle favorite |
| `[` | Move queue item up |
| `]` | Move queue item down |
| `a` | Connect account |
| `m` | Load more results |
| `d` | Recheck dependencies |
| `c` | Choose country |
| `L` | Toggle lyrics |
| `Q` | Toggle compact queue |
| `Esc` | Close or cancel |
| `Backspace` | Delete previous character |
| `Enter` | Activate selected row / submit text |

`Tab` and `Shift-Tab` cycle focus among Navigation, Content, and Player; Queue
is intentionally skipped. While Navigation is focused, `Left`/`Right` (or
`h`/`l`) immediately opens the previous or next destination. This wraps across
Home, Search, Charts, Podcasts, Library, Favorites, History, and Settings in
both directions. Favorites is a top-level destination after Library and before
History.

The player renders Previous, rewind, Play/Pause, fast-forward, and Next as
visible button-style labels. The visible button-style labels are keyboard
shortcuts, not a separate focus mode: use the keys printed in each label. Mouse
users can also click a visible enabled label. `F7` / `F8` / `F9` and native
Media Previous, Play/Pause, and Next keys perform the same three transport
actions when the terminal reports those keys. `Shift+Left` and `Shift+Right`
seek backward and forward. Music seeks by a fixed 10 seconds; podcasts use the
configured skip interval from `[podcast]`.

Mouse clicks work on Navigation, visible list rows, player controls, and the
progress bar. For Favorites and other visible lists, the first click selects an
unselected row; clicking the already-selected row again activates it. On
Favorites, that second click starts Favorites list playback. Mouse clicks also
select and then activate choices in the browser, country, and command-palette
overlays. The mouse wheel moves lists and scrollable overlays. Only geometry
from the latest completed frame is active, so clipped, stale, loading, and
disabled rows do not become click targets.

The player uses a theme-aware borderless animated progress bar. Pausing freezes
progress fill and shimmer at the authoritative playback position; resuming or
seeking converges from that position without changing playback state. Mouse
seeking remains supported across the complete progress bar.

Selectable lists use a gliding cursor while retaining a distinct selected-row
style. List animation is presentation-only: logical selection changes
immediately, so keyboard and mouse activation always targets the selected row,
not the cursor's transient position. Visible loading states use a Braille
spinner. A single bounded, coalesced motion clock drives these effects at no
more than 30 FPS, and the motion clock idles when nothing visible needs
animation.

Charts keeps its list viewport stable when selection moves within the visible
window, including across section boundaries; the screen scrolls only when the
selected song reaches an edge.

## Native now-playing notifications

Native now-playing notifications are enabled by default and show bounded title,
artist, and collection metadata when a new track starts playing. Set
`notifications.enabled = false` to disable them. Notification setup, artwork,
timeouts, or platform failures never block playback; unavailable platforms
silently retain normal playback after a generic warning.

macOS and Linux can attach artwork using a bounded private PNG cache. The cache
retains at most two artwork files, uses private permissions on Unix, and removes
leftovers at every TUI startup, even when notifications are disabled or
unavailable. Preparation runs on a dedicated OS thread with a 100 ms startup
bound; on cache error or timeout the TUI continues with text-only notifications
when possible. Windows notifications require an optional,
already-registered `notifications.windows_aum_id`; without it the default is
notification-unavailable. Windows notifications are text-only. Ytermusic makes
no registry or PowerShell changes and never attempts to register the ID itself.
Artwork URLs and provider IDs are never logged, and notification request debug
output redacts titles, artists, collection names, URLs, and file paths.

## Lyrics, animated artwork, and audio spectrum

Time-synchronized lyrics appear automatically in the player when a conservative
match is available: the wide layout shows the previous, highlighted current,
and next lines, while compact and tiny layouts show only the current line when
space permits. The highlight uses a timestamp-derived fade between consecutive
lines, recomputes after a seek, and freezes with the playback position while
paused; it does not add a background timer. The player highlights the active
synchronized line even outside the transition window. `L` opens and closes the
full lyrics overlay. In that overlay,
`j` / `k` or the arrow keys scroll manually, `Enter` resumes follow mode and
recenters the active line, and `L` or `Esc` closes the overlay. When only plain
lyrics are available, they stay out of the player and appear in the full
overlay instead. The overlay attributes its source as YouTube Music or LRCLIB.

Lyrics are bounded and cached in memory only for the current process; lyric
text is not written to logs. Before storage and terminal-width measurement,
tabs and timed-line breaks are normalized to spaces, while other control
characters are removed; plain-lyrics line breaks remain structural. YouTube
Music supplies plain lyrics. With
`lyrics.external_sync = true` (the default), Ytermusic sends the bounded track
title, artist, and album when available to LRCLIB over HTTPS, using a strict
full-title, artist, and album search first. If that returns no match, it may
send one bounded complete-title request without artist or album metadata before
up to three bounded exact title segments, also without artist or album metadata.
Duration is used locally to require a unique conservative match and is not sent.
Setting `lyrics.external_sync = false` disables all LRCLIB requests while
retaining YouTube Music plain lyrics; set `lyrics.enabled = false` to disable all
lyric retrieval and presentation.

For eligible videos, genuine low-resolution video frames can appear as
animated artwork in the wide layout only. This requires a low-resolution
preview from `yt-dlp` and a working FFmpeg executable. Wide uses animated
artwork when available and otherwise static artwork; Compact uses static
artwork only, and Tiny omits artwork. Missing previews, FFmpeg failures, pause,
or unsupported preview formats retain the last safe wide frame or use the
existing static artwork fallback without interrupting audio. Set
`artwork.animated = false` to disable this feature.
`artwork.max_fps` accepts `1-15`; the default is 8. Frame size, frame rate,
process lifetime, decoded bytes, and subprocess output are bounded to limit CPU
and memory use. Preview URLs and frame bytes are never logged, and decoded
frames are memory only and discarded when the process exits.

The player also shows a genuine audio-reactive spectrum when analysis is
available. The wide layout reserves three rows, the compact layout reserves one
row, and the tiny layout keeps its existing one-row player without a spectrum.
The theme-derived spectrum gradient moves from accent through foreground to
brighter foreground as bands rise, with bounded brightness for louder levels.
It degrades deterministically to the terminal's color capability. `NO_COLOR`,
non-terminal or dumb output uses monochrome; ordinary ANSI terminals use the
Basic 16-color palette, 256-color terminals use ANSI 256, and declared 24-bit
terminals use true color. Paused or failed playback is muted. The strip has its
own bounded space between player controls and synchronized
lyrics, so it cannot cover artwork, progress, volume, playback modes, controls,
or lyric text. Low-frequency bands use the theme accent. Pausing freezes and
dims the newest frame.

Spectrum analysis requires FFmpeg and performs a separate low-bandwidth audio
decode of the current resolved stream; it does not inspect synthetic progress
or mpv logs. This costs some additional CPU and network bandwidth while music
is playing. Analysis input, decoded samples, and frames stay in memory, and
stream URLs are never logged. A missing dependency, unsupported stream, or
analysis failure falls back to a quiet baseline and never interrupts audio.

The visualizer is enabled by default and is independent of animated artwork:

```toml
[visualizer]
enabled = true
max_fps = 15
```

Set `visualizer.enabled = false` to preserve the original player heights and
disable the extra decode. `visualizer.max_fps` accepts `1-30`, caps spectrum
publication and redraw cadence, and the default is 15 frames per second. The
mono 8 kHz decode and FFT cadence remain fixed; process runtime and I/O remain
bounded separately.

## Country charts

Press `c` to choose a two-letter country code, then browse that region's
country-specific chart. The country setting is shared: `c` refreshes both Charts
and Podcasts without requiring an account. The configured `ZZ` default means
unspecified; `ZZ` detects the OS locale and falls back to `US` when no supported
country can be detected. Choose a country in the TUI or set `region` in
`config.toml` when a fixed region is preferred.

## Podcasts and resume

Podcasts opens with country Top Shows when no show is open. These rankings use
Apple's public Top Shows metadata. Opening or loading rankings makes a direct
unauthenticated request to Apple using the selected or detected country. Apple
provides discovery metadata only, not playback links; YouTube Music remains
responsible for search, show details, and playback.

Select a ranked show and press `Enter` to lazily match the selected show on
YouTube Music, then open its episodes. `/` remains available for manual search,
and `Esc` returns from an opened show to the rankings. If rankings cannot be
loaded, Podcasts remains usable and manual search remains available.

Ranking recommendations are cached in process memory only for the current
session and remain fresh for about one hour. Episode progress and the current
session resume automatically by default, so reopening Ytermusic returns to the
saved position. Podcast speed and the backward and forward skip intervals are
configurable; see `config.example.toml` for valid values.

## Favorites and list playback

Favorites is a top-level destination after Library. Favorites are local to this
machine and database, loaded from `ytermusic.db` at startup, and shown newest
first. Normal-mode `f` toggles the selected playable item when Content is
focused on Search, Charts, an opened podcast episode list, Library song lists,
History, or Favorites. Queue focus toggles the selected queue item; Player focus
toggles the displayed/current playback media. Navigation, metadata,
unsupported, and empty targets do nothing. During Search or command-palette
text entry, `f` types normally.

Activating a playable row in Search, Charts, History, Favorites, a Library song
list, or an opened podcast episode list atomically replaces the queue with all
currently loaded playable rows and starts the selected item. Metadata rows and
duplicate full media IDs are excluded. At most 1,024 unique playable rows are
accepted; 1,025 or more are rejected. On rejection, the old queue, playback,
and modes are preserved and a safe error is displayed. Repeat mode is preserved.
With shuffle enabled, the selected item stays current while the remaining items
are randomized. Explicit list playback disables Endless Radio.

Favorites are capped at 1,024. Overflow is visibly rejected without eviction.
Removing the playing favorite does not stop playback or remove it from the
queue. Favorites persist across app restarts and remain independent of session
and queue resets.

## Queue, radio, and fades

The queue plays in sequential order by default. Press `s` to toggle random
shuffle order, `r` to cycle repeat off, repeat one, and repeat all, and `e` to
toggle endless radio recommendations. Queue changes are persisted with the
session. Configurable fade-in and fade-out durations soften starts, stops, skips,
and track transitions.

## Browser session privacy

Browser-session import is optional and only adds account-backed library
features. Ytermusic never asks for a Google password. To connect an already
signed-in supported browser, use one of its explicit names:

```text
ytermusic auth import firefox
ytermusic auth status
```

During import, `yt-dlp` writes a restricted temporary cookie export. Ytermusic
reads only bounded YouTube authentication cookies, verifies the session, and
removes the temporary cookie file. The minimum cookie header needed for future
sessions is stored in the operating system credential vault, never in
`config.toml`, SQLite, logs, crash output, or diagnostics. Use
`ytermusic auth logout` to remove it from the credential vault.

## Files and paths

Ytermusic follows each operating system's per-user application directories.
Environment variables below use their standard fallback when unset.

| Platform | Config | Data | Cache | Log |
| --- | --- | --- | --- | --- |
| macOS | `~/Library/Application Support/dev.ytermusic.ytermusic/config.toml` | `~/Library/Application Support/dev.ytermusic.ytermusic/ytermusic.db` | `~/Library/Caches/dev.ytermusic.ytermusic/` | `~/Library/Application Support/dev.ytermusic.ytermusic/logs/ytermusic.log` |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/ytermusic/config.toml` | `${XDG_DATA_HOME:-~/.local/share}/ytermusic/ytermusic.db` | `${XDG_CACHE_HOME:-~/.cache}/ytermusic/` | `${XDG_DATA_HOME:-~/.local/share}/ytermusic/logs/ytermusic.log` |
| Windows | `%APPDATA%\ytermusic\ytermusic\config\config.toml` | `%APPDATA%\ytermusic\ytermusic\data\ytermusic.db` | `%LOCALAPPDATA%\ytermusic\ytermusic\cache\` | `%APPDATA%\ytermusic\ytermusic\data\logs\ytermusic.log` |

Copy `config.example.toml` to the platform config path to customize defaults.
The chart metadata cache is stored in `ytermusic.db` alongside the saved
session, playback history, and podcast progress. Local Favorites are stored in
that same database. Player artwork and stream-resolver caches are memory-only
and are discarded when Ytermusic exits.
The notification artwork cache retains at most two private PNG files under the
platform cache directory so the operating system can consume them after native
submission, and removes notification artwork leftovers at startup.

Chart rows expire automatically, so there is no separate on-disk chart cache to
clear. Do not delete or move `ytermusic.db` while Ytermusic is running. If
recovery requires replacing or removing the database, quit Ytermusic and make a
backup first: that affects the session, playback history, podcast progress, and
cached chart metadata together. It also affects local Favorites.

## Troubleshooting

Run:

```text
ytermusic doctor
```

The report checks `mpv`, `yt-dlp`, `ffmpeg`, their required capabilities, and
whether playback is available. It does not check `deno`. Verify the JavaScript
runtime and resolver separately:

```text
deno --version
yt-dlp --ignore-config --version
```

If a dependency is missing, install it with the platform command above, ensure
its executable is on `PATH`, and rerun the checks. Browsing remains available
when playback dependencies are unhealthy. For playback failures after a
healthy report, inspect `ytermusic.log` at the platform path above; secret
browser material is excluded from logs.

## Project status and media policy

Ytermusic is an unofficial community project and is not affiliated with YouTube or Google.
It streams only and never downloads media. It does not provide a
permanent-save or offline-media feature. Users are responsible for following
the service terms and laws that apply to them.

Ytermusic is released under the MIT License; see `LICENSE`.
