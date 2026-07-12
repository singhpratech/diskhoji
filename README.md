<p align="center"><img src="docs/icon.png" width="128" alt="Diskhoji — Varuna's pasha binding a treemap"></p>

# ▦ Diskhoji

<p align="center"><img src="docs/hero.jpg" alt="Sailing the digital seas under Varuna's watch" width="820"></p>
<p align="center"><a href="https://diskhoji.org"><b>diskhoji.org</b></a> · <a href="https://github.com/singhpratech/diskhoji/releases">Download</a></p>

<p align="center">
  <img alt="release" src="https://img.shields.io/github/v/release/singhpratech/diskhoji?color=E5B96B&labelColor=141922">
  <img alt="license" src="https://img.shields.io/badge/license-MIT-3987e5?labelColor=141922">
  <img alt="platforms" src="https://img.shields.io/badge/platforms-Linux%20·%20macOS%20·%20Windows-199e70?labelColor=141922">
  <img alt="downloads" src="https://img.shields.io/github/downloads/singhpratech/diskhoji/total?color=E5B96B&labelColor=141922">
</p>

**Fast, beautiful disk space analysis for Linux, macOS, and Windows.** One standalone ~6 MB binary that opens a true native window — no Electron, no webview, no runtime dependencies. A fully parallel Rust scanner, three visualizations (cushion treemap + heatmap grid + radial rings), light & dark themes, and an optional `--web` mode that serves the same dashboard on localhost.

*khoji (खोजी) — seeker. Every byte, accounted for.*

## Why

The great treemap disk analyzers were an inspiration — but they were Windows-only, single-threaded, and never made the jump. Diskhoji is not a port or a clone of any of them: it's a ground-up rebuild in Rust, designed for how disks (and screens) work today.

## What stands Diskhoji apart

- **One standalone binary, truly native.** The executable *is* the app: scanner, treemap engine, and a real native window (egui) in a single ~6.2 MB file. No installer, no Electron's 200 MB, no webview library. Copy it to any machine and run it.
- **Genuinely fast.** The scanner is work-stealing parallel across every core — measured at **1.85 million files / 681 GiB catalogued in ~2.7 s cold, ~0.4–0.7 s warm** (≈690k files/s cold, 3–4.5M files/s warm). Rescans are painless, so you actually do them.
- **The heavy lifting stays native.** The squarified treemap layout is computed in Rust in ~15 ms for a 2-million-node tree; the UI only paints pre-computed rectangles. Million-file folders never freeze the UI.
- **Three readings of the same disk.**
  - *Treemap* — cushion-shaded, area = size: instantly shows where the bulk is.
  - *Heatmap* — a contributions-style grid: every item gets an equal cell, color = size on a log scale. Small-but-numerous files stay visible instead of vanishing into slivers.
  - *Rings* — a radial sunburst: the current folder at the hub, each ring a level deeper, arc = size. The shape of the whole hierarchy at a glance. Toggle any of the three in the map header.
- **A real dashboard, fully cross-linked.** Explorer tree ⟷ map ⟷ file types ⟷ largest files: click a type to spotlight it in the map, click a large file to locate it in the tree, zoom with breadcrumbs and double-click.
- **Delete without rescanning.** Deleting updates the in-memory model in place — every panel, total, and chart is correct immediately.
- **Private by design.** By default the native app makes no network connection at all — no telemetry, no trackers, no account, nothing about you or your files leaves the machine. Update checks are strictly opt-in (off by default); when you enable them or click *Check for updates*, Diskhoji only compares your version against GitHub and sends nothing else. (`--web` mode binds `127.0.0.1` and nowhere else.)
- **Safety rails.** Stays on one filesystem (never wanders into `/proc`, other mounts, or network fs), never follows symlinks, and refuses to delete the scan root.

## Install

**One line** (prebuilt binaries: Linux x86_64 · macOS Apple Silicon · Windows x86_64 — Intel Macs build from source via cargo):

```sh
curl -fsSL https://diskhoji.org/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://diskhoji.org/install.ps1 | iex
```

**Or download by hand** from [Releases](https://github.com/singhpratech/diskhoji/releases):

```sh
tar xzf diskhoji-*-linux-x86_64.tar.gz
./diskhoji                           # opens the native app
./diskhoji ~/                        # scan a path immediately
```

On **macOS** grab `Diskhoji-*-macos-arm64.dmg` (drag Diskhoji into Applications) or the `.AppImage` on **Linux** for a double-click launch. On **Windows** run `diskhoji-*-windows-x86_64.msi` (installs with Start Menu & Desktop shortcuts) or unzip `diskhoji-*-windows-x86_64.zip` for the portable `.exe`.

**Or build from source** (Linux, macOS, Windows — needs the [Rust toolchain](https://rustup.rs)):

```sh
git clone https://github.com/singhpratech/diskhoji && cd diskhoji
cargo build --release
./target/release/diskhoji [PATH] [--web] [--port N] [--no-open]
```

Diskhoji opens a native window by default. `--web` serves the same dashboard on `127.0.0.1` for your browser instead.

**macOS first run (Apple Silicon):** open the `.dmg` and drag **Diskhoji** into Applications. It's ad-hoc signed but not yet notarized, so the first launch is blocked ("Apple could not verify Diskhoji is free of malware"). Clear it once — either open it and choose **System Settings → Privacy & Security → Open Anyway**, or in Terminal:

```sh
xattr -dr com.apple.quarantine /Applications/Diskhoji.app
open /Applications/Diskhoji.app
```

**Windows first run:** the build is not yet code-signed, so SmartScreen may say "Windows protected your PC". Click **More info → Run anyway** (or right-click the file → Properties → **Unblock**).

## What it does

- **Parallel scanner** — rayon work-stealing across all cores, live progress while it runs.
- **Cushion treemap** — click to select, double-click to zoom, breadcrumbs to climb back out.
- **Heatmap grid** — one cell per item, ring = folder, color = size (log); same zoom and right-click actions.
- **Radial rings** — a sunburst of the hierarchy: current folder at the hub, up to five rings deep, arc = size; same zoom and right-click actions.
- **Explorer tree** — sizes, percent bars, sorted by weight, expand-on-demand.
- **File types** — top extensions by bytes with a fixed, CVD-validated categorical palette.
- **Largest files** — top 15, click to locate in the tree.
- **Right-click anywhere** (tree row, map tile, heat cell, ring arc, largest-files list):
  - Open · Reveal in file manager · Copy path · Zoom · **Delete permanently** (confirmed, bypasses trash, updates every panel in place)
- **Keyboard** — `Backspace` zoom up · `Delete` delete selection · `Esc` dismiss.

## Design notes

- Native window by default (light & dark themes, `A+`/`A−` text zoom). `--web` mode embeds the whole dashboard as a single HTML file served on `127.0.0.1` only.
- Sizes are logical file sizes, shown in binary units (KiB/MiB/GiB).
- Symlinks are counted as their own link size and never followed.
- Deleting the scan root is refused.

## API (`--web` mode)

The web dashboard talks to a tiny JSON API you can script against:
`/api/status` · `/api/roots` · `/api/scan` · `/api/summary` · `/api/node/{id}` · `/api/treemap?id&w&h` · `/api/delete` · `/api/reveal` · `/api/open`

## License

MIT — see [LICENSE](LICENSE).

---

<p align="center"><img src="docs/icon.png" width="56" alt=""></p>
<p align="center"><i>khoji (खोजी) — seeker.</i></p>
<p align="center"><b>Every byte, accounted for.</b></p>
<p align="center"><a href="https://diskhoji.org">diskhoji.org</a> · <a href="https://github.com/singhpratech/diskhoji/releases">Releases</a> · <a href="https://github.com/singhpratech/diskhoji/issues">Issues</a></p>
