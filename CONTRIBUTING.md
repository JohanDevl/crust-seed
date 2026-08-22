# Contributing to crust-seed

## Branches

- `develop` — integration branch. All work lands here first.
- `main` — release branch. Only ever updated by merging `develop`.

Open feature branches off `develop` and PR back into it. When `develop` is in
a state worth shipping, PR `develop` → `main`; merging that is what publishes
`:latest` and cuts a release.

## Commit messages

crust-seed does **not** use Conventional Commits. A subject line is a short
imperative sentence saying what the commit does, capitalised, no prefix and no
trailing period:

```
Resolve XML entity references instead of dropping them
Treat seasonFromEpisodes = 0 as disabled
Give the qBittorrent API key its own field in the Web UI
```

The body is where the value is. Explain *why* the change was needed and what
you ruled out — the diff already says what changed. When a change looks
arbitrary out of context (a magic number, a deliberate divergence from
cross-seed, a workaround for a library quirk), that reasoning belongs in the
commit message, and usually in a code comment too.

Do not add tool attribution footers.

## Building and checking your work

```bash
# Web UI — must be built before the binary embeds it
npm -C web ci
npm -C web run build

cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

Node 26+ and the pinned stable Rust toolchain (`rust-toolchain.toml`, MSRV
1.95).

What CI actually runs is narrower than that, so run the full set locally:

- **Pull requests** (`ci.yml`) — the three cargo commands, and `npm ci` +
  `npm run build` for the web workspaces.
- **Pushes to `main` / `develop`** (`docker.yml`) — the three cargo commands
  only; the web UI is built inside the Docker image. `build-and-push` needs
  `check`, so nothing is published unless they pass.

Neither runs `typecheck`, `lint` or the vitest suite. Run them yourself when
you have touched anything under `web/`:

```bash
npm -C web run typecheck
npm -C web run lint
npm -C web/webui exec vitest run
```

### Working on the web UI

The vendored SPA is embedded into the binary by `rust-embed`. In a debug build
it is read from `web/webui/dist` at request time, so `npm -C web run build`
against a running `cargo run -- daemon` is enough to see a change — no Rust
rebuild. Release builds bake it in.

For live reload, run Vite against a daemon on the default port:

```bash
cargo run -- daemon           # terminal 1, port 2468
npm -C web/webui run dev      # terminal 2, port 5173
```

Vite proxies `/api/trpc` and `/api/dev-login` to port 2468 — enough for the UI,
but not the whole `/api` surface. `crust-seed dev-login` prints a URL that logs
you in without the signup window; it needs `CRUST_SEED_DEV_LOGIN=true` on the
daemon.

`web/webui/src/routeTree.gen.ts` is generated and git-ignored; the `prebuild`
and `prelint` hooks regenerate it.

## Design tokens

Colours, radii and shadows live in `web/webui/src/index.css`. Both schemes are
defined in full, and **every** colour in the app resolves through a token —
`bg-success`, `text-destructive`, `border-warning/40`. Do not reach for raw
Tailwind palette classes (`bg-red-500`, `text-green-600`): they ignore the
theme, and a sweep to remove the last of them is what made dark mode coherent.

New status colours need a light *and* a dark value, and the light one has to
clear 4.5:1 as text on `--card`. The vivid ambers and greens that look right
in dark mode do not — that is why the light scheme uses deeper shades of the
same hues.

## Artwork

The mark lives in four places, all the same geometry:

| File | Used by |
| ---- | ------- |
| `web/webui/src/components/brand/LogoMark.tsx` | sidebar, login, loading state |
| `web/webui/public/crust-seed.svg` | `<link rel="icon">` |
| `web/webui/public/favicon.ico` | legacy favicon (16/32/48) |
| `docs/logo/crust-seed.{svg,png}` | README |

If you change the artwork, change all four. The rasters are generated from the
SVG:

```bash
magick -background none docs/logo/crust-seed.svg -resize 512x512 docs/logo/crust-seed.png
cp docs/logo/crust-seed.svg web/webui/public/crust-seed.svg
magick -background none docs/logo/crust-seed.svg \
  \( -clone 0 -resize 16x16 \) \( -clone 0 -resize 32x32 \) \( -clone 0 -resize 48x48 \) \
  -delete 0 web/webui/public/favicon.ico
```

## Releasing

Releases are cut by pushing to `main`; there is nothing to tag by hand.

1. Bump `version` in `Cargo.toml` on `develop` and commit it. This is the
   single source of truth for the image tag, the git tag and the release.
2. PR `develop` → `main` and merge it.
3. `docker.yml` runs fmt, clippy and the test suite, then builds and pushes
   `ghcr.io/johandevl/crust-seed:vX.Y.Z` and `:latest`, creates the `vX.Y.Z`
   git tag, and opens a GitHub release with generated notes.
4. Back-merge into `develop` if `main` gained anything:

   ```bash
   git checkout develop && git merge --ff-only origin/main && git push
   ```

The release step is idempotent: pushing to `main` without bumping
`Cargo.toml` republishes `:latest` and skips the tag, so a docs-only merge
stays release-less by design. Pushes to `develop` publish `:develop` only.

## Scope

crust-seed is a port. Behavioural parity with cross-seed 7.x is the constraint
that decides most questions — if cross-seed does something surprising, the
default is to do the same thing and say why in a comment.

Where a divergence is genuinely better, it needs to be deliberate, documented
in the code, and listed under **Differences from cross-seed** in the README.
There are three today. Adding a fourth is a design decision, not an
implementation detail.

Some identifiers still say *cross-seed* and must stay that way: the qBittorrent
tag and category suffix, the default `linkCategory`, the `outputDir` name and
the session cookie. They name things that already exist in users' clients and
on their disks.
