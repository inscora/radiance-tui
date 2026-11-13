# radiance-tui

Live [radiance](https://codeberg.org/StillDeadcode/radiance) engine stats in the Linux console — a terminal dashboard in the
spirit of `strata monitor`, built with Rust + [ratatui](https://ratatui.rs).

![radiance-tui dashboard](docs/screen.jpg)

One screen, refreshed every second:

- **Header** — model, engine version, uptime, health dot (serving / stale / down),
  running / waiting / queue depth, total requests
- **decode / prefill** — live tok/s sparklines: 4 Hz sampling with peak-hold decay, so even a few-token burst paints a visible band (60 s window)
- **engine** — draft acceptance rate, preemptions
- **kv cache** — utilization gauge (tokens used / capacity)
- **cache** — carved KV VRAM, prefix & linear prefix-cache hit rates
- **experts / throughput** — MoE models: layer×expert residency, promote/demote
  churn, starved slab. Dense models (no `experts` in `/stats`): live
  prompt/output token totals, avg TTFT, and average prefill & decode
  tok/s (averaged over active time only — zero-rate samples are excluded)
- **cards** — per-GPU VRAM bars
- **requests** — live table: state, slot, prompt/cached/output tokens, ctx,
  TTFT, prefill time, per-request tok/s, age (color-coded by state)

## Build

```sh
cargo build --release
```

Arch Linux package (self-contained, offline build with vendored crates):

```sh
./packaging/make-package.sh    # -> packaging/radiance-tui-<ver>-1-<arch>.pkg.tar.zst
sudo pacman -U packaging/radiance-tui-*.pkg.tar.zst
```

## Run

```sh
# key also read from $RADIANCE_API_KEY
RADIANCE_API_KEY=... ./target/release/radiance-tui --url http://10.13.37.149:8080

# one-shot text snapshot, no TUI (scripting / cron)
./target/release/radiance-tui --url ... --key ... --once
```

The port is just part of `--url` — the default is 8080. To target a
different port, only the port changes: `--url http://<host>:9090`.

| flag        | default               | meaning                          |
|-------------|-----------------------|----------------------------------|
| `--url`     | `http://127.0.0.1:8080` | engine base URL (default port 8080) |
| `--key`     | `$RADIANCE_API_KEY`   | bearer key for `/stats`          |
| `--interval`| `250`                 | poll interval, ms                |
| `--once`    | —                     | print one snapshot and exit      |

Keys in the TUI: `q` quit · `space` freeze/resume the frame · `s` poll now.

## Data sources

- `GET /health` (unauthenticated) — liveness, model, in-flight/queue
- `GET /stats` (bearer key) — everything else, JSON (radiance ≥ 1.17)

Missing fields degrade gracefully; a 401 shows a help modal instead of dying.
