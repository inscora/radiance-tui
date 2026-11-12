# radiance-tui

Live [radiance](http) engine stats in the Linux console — a terminal dashboard in the
spirit of `strata monitor`, built with Rust + [ratatui](https://ratatui.rs).

![radiance-tui dashboard](docs/screen.jpg)

One screen, refreshed every second:

- **Header** — model, engine version, uptime, health dot (serving / stale / down),
  running / waiting / queue depth, total requests
- **decode / prefill** — live tok/s with 2-minute sparkline history
- **engine** — draft acceptance rate, preemptions
- **kv cache** — utilization gauge (tokens used / capacity)
- **cache** — carved KV VRAM, prefix & linear prefix-cache hit rates
- **experts** — MoE layer×expert residency, promote/demote churn, starved slab
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

| flag        | default               | meaning                          |
|-------------|-----------------------|----------------------------------|
| `--url`     | `http://127.0.0.1:8080` | engine base URL                |
| `--key`     | `$RADIANCE_API_KEY`   | bearer key for `/stats`          |
| `--interval`| `1000`                | poll interval, ms                |
| `--once`    | —                     | print one snapshot and exit      |

Keys in the TUI: `q` quit · `space` freeze/resume the frame · `s` poll now.

## Data sources

- `GET /health` (unauthenticated) — liveness, model, in-flight/queue
- `GET /stats` (bearer key) — everything else, JSON (radiance ≥ 1.17)

Missing fields degrade gracefully; a 401 shows a help modal instead of dying.
