# crust-seed

Fully-automatic cross-seeding with Torznab — a Rust rewrite of
[cross-seed](https://github.com/cross-seed/cross-seed).

crust-seed finds torrents you can cross-seed from content you already have. It
searches your indexers for releases that match your existing downloads, and
either saves the `.torrent` files or injects them straight into your client,
linking the data into place so nothing is downloaded twice.

## What it does

- **Search** every torrent in your client (or on disk) against your Torznab
  indexers, on a schedule or on demand.
- **RSS** — poll each indexer's recent releases and match them as they appear.
- **Announce/webhook** — match a single release the moment autobrr sees it.
- **Inject** matched torrents into qBittorrent, rTorrent, Transmission or
  Deluge, hardlinking/symlinking/reflinking the data so the client can seed it
  without a second copy.
- **Web UI** for configuration, indexer management, statistics, health checks
  and live logs.

## Requirements

- Any indexers that support Torznab (via Prowlarr or Jackett)
- At least one torrent client: qBittorrent, rTorrent, Transmission or Deluge

## Running with Docker

```bash
docker run -d \
  --name crust-seed \
  -p 2468:2468 \
  -v /path/to/config:/config \
  -v /path/to/torrents:/torrents \
  -v /path/to/data:/data \
  -v /path/to/links:/links \
  ghcr.io/johandevl/crust-seed:latest
```

The container runs as UID/GID `65532:65532`; `chown` the config volume to match.

Then open <http://localhost:2468> and create the first user. The signup window
closes five minutes after startup — restart the container if you miss it.

Image tags:

| Tag        | Points at                              |
| ---------- | -------------------------------------- |
| `latest`   | the most recent build of `main`        |
| `vX.Y.Z`   | a specific release                     |
| `develop`  | the most recent build of `develop`     |

## Running from source

```bash
# Build the web UI (needs Node 26+)
npm -C web ci
npm -C web run build

# Build and run
cargo build --release
./target/release/crust-seed daemon
```

The web UI is embedded into the binary at compile time. Building without it
produces a working daemon whose UI shows a placeholder page.

## Configuration

Everything is configurable through the Web UI, which is the source of truth.
For an initial setup you can drop a [`config.toml`](config.example.toml) into
the config directory (`/config` in Docker, `~/.crust-seed` otherwise); it is
imported into the database on first run and then renamed aside.

## Commands

```
crust-seed daemon                        # web UI + scheduled jobs
crust-seed search                        # one-off search of everything
crust-seed rss                           # one-off RSS scan
crust-seed inject                        # inject saved .torrent files
crust-seed restore                       # restore the torrent cache to outputDir
crust-seed diff <a.torrent> <b.torrent>  # explain why two torrents do or don't match
crust-seed tree <torrent>                # print a torrent's file tree
crust-seed api-key                       # show (or set) the API key
crust-seed --help                        # everything else
```

## API

With the API key in an `X-Api-Key` header or an `apikey` query parameter:

| Endpoint            | Purpose                                    |
| ------------------- | ------------------------------------------ |
| `POST /api/announce` | match a single release (autobrr)          |
| `POST /api/webhook`  | search for one torrent by infoHash or path |
| `POST /api/job`      | run a job ahead of schedule                |
| `GET  /api/status`   | authenticated health check                 |
| `GET  /api/ping`     | unauthenticated health check               |
| `/api/indexer/v1/*`  | indexer CRUD (Prowlarr integration)        |

## Differences from cross-seed

crust-seed aims to do the same things as cross-seed 7.x, and vendors its React
web UI unchanged. Two things are deliberately different:

- **Configuration file format.** cross-seed's `config.js` is executable
  JavaScript that its Node daemon evaluates. crust-seed reads a declarative
  `config.toml` (or `config.json`) with the same option names and the same
  `ms`-style duration strings. See [`config.example.toml`](config.example.toml).
- **No in-place upgrade from an existing `cross-seed.db`.** crust-seed creates
  its own database and rebuilds its caches from your client and data
  directories on first run. Search history is not carried over.

## Credits

cross-seed is the work of [Michael Goodnow](https://github.com/mmgoodnow) and
its contributors. This is a port of their design and their matching logic to
Rust; the web UI is theirs, vendored under `web/`.

## License

Apache-2.0, the same as cross-seed.
