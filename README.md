<p align="center"><img src="docs/icon.png" width="128" alt="Diskhoji — Varuna's pasha binding a treemap"></p>

# ▦ Diskhoji

**Fast, beautiful disk space analysis for Linux and macOS.** One standalone ~6 MB binary that opens a true native window — no Electron, no webview, no runtime dependencies. A fully parallel Rust scanner, two visualizations (cushion treemap + heatmap grid), light & dark themes, and an optional `--web` mode that serves the same dashboard on localhost.

*khoji (खोजी) — seeker. Every byte, accounted for.*

## Why

The great treemap disk analyzers were an inspiration — but they were Windows-only, single-threaded, and never made the jump. Diskhoji is not a port or a clone of any of them: it's a ground-up rebuild in Rust, designed for how disks (and screens) work today.

## What stands Diskhoji apart

- **One standalone binary, truly native.** The executable *is* the app: scanner, treemap engine, and a real native window (egui) in a single ~6 MB file. No installer, no Electron's 200 MB, no webview library. Copy it to any machine and run it.
- **Genuinely fast.** The scanner is work-stealing parallel across every core — measured at **1.85 million files / 681 GiB catalogued in ~2.7 s cold, ~0.4 s warm** (≈690k–4.9M files/s). Rescans are painless, so you actually do them.
- **The heavy lifting stays native.** The squarified treemap layout is computed in Rust in ~15 ms for a 2-million-node tree; the UI only paints pre-computed rectangles. Million-file folders never freeze the UI.
- **Two readings of the same disk.**
  - *Treemap* — cushion-shaded, area = size: instantly shows where the bulk is.
  - *Heatmap* — a contributions-style grid: every item gets an equal cell, color = size on a log scale. Small-but-numerous files stay visible instead of vanishing into slivers. Toggle in the map header.
- **A real dashboard, fully cross-linked.** Explorer tree ⟷ map ⟷ file types ⟷ largest files: click a type to spotlight it in the map, click a large file to locate it in the tree, zoom with breadcrumbs and double-click.
- **Delete without rescanning.** Deleting updates the in-memory model in place — every panel, total, and chart is correct immediately.
- **Private by design.** Binds to `127.0.0.1` only. No telemetry, no network, nothing ever leaves the machine.
- **Safety rails.** Stays on one filesystem (never wanders into `/proc`, other mounts, or network fs), never follows symlinks, and refuses to delete the scan root.

## Install

**One line** (Linux x86_64 gets the prebuilt binary; macOS builds from source):

```sh
curl -fsSL https://diskhoji.org/install.sh | sh
```

**Or download by hand** from [Releases](https://github.com/singhpratech/diskhoji/releases):

```sh
tar xzf diskhoji-*-linux-x86_64.tar.gz
./diskhoji                           # opens the native app
./diskhoji ~/                        # scan a path immediately
```

**Or build from source** (any Linux, macOS — needs the [Rust toolchain](https://rustup.rs)):

```sh
git clone https://github.com/singhpratech/diskhoji && cd diskhoji
cargo build --release
./target/release/diskhoji [PATH] [--web] [--port N] [--no-open]
```

Diskhoji opens a native window by default. `--web` serves the same dashboard on `127.0.0.1` for your browser instead.

## What it does

- **Parallel scanner** — rayon work-stealing across all cores, live progress while it runs.
- **Cushion treemap** — click to select, double-click to zoom, breadcrumbs to climb back out.
- **Heatmap grid** — one cell per item, ring = folder, color = size (log); same zoom and right-click actions.
- **Explorer tree** — sizes, percent bars, sorted by weight, expand-on-demand.
- **File types** — top extensions by bytes with a fixed, CVD-validated categorical palette.
- **Largest files** — top 15, click to locate in the tree.
- **Right-click anywhere** (tree row, map tile, heat cell, largest-files list):
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

**[diskhoji.org](https://diskhoji.org)**
