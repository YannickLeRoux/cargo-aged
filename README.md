# cargo-aged

A Cargo subcommand that updates dependencies **only when their latest stable release has aged past a configurable threshold**. Useful for teams that want to avoid pulling in freshly-published crate versions before the ecosystem has had a chance to shake out bugs, supply-chain issues, or yanks.

For each dependency in your `Cargo.toml`, `cargo-aged` queries the crates.io API for the newest non-prerelease, non-yanked version. If that version was published at least *N* days ago, it runs `cargo update -p <crate> --precise <version>` to pin your `Cargo.lock` to it.

## Install

From source (from this repo):

```sh
cargo install --path .
```

Or install directly from a git checkout:

```sh
cargo install --git https://github.com/YannickLeRoux/cargo-aged.git
```

Once installed, the `cargo-aged` binary lives in `~/.cargo/bin/`, which Cargo picks up as the `cargo aged` subcommand.

## Usage

Run inside any Cargo project:

```sh
cargo aged                       # default: 30-day minimum age
cargo aged --min-age 14          # more aggressive
cargo aged --min-age 90 --dry-run
```

### Configuring `min-publish-age`

`cargo-aged` reads the same config key that upcoming Cargo `-Zmin-publish-age` support ([tracking issue #17009](https://github.com/rust-lang/cargo/issues/17009), [RFC #3923](https://github.com/rust-lang/rfcs/pull/3923)) uses. Put this in your project's `.cargo/config.toml`:

```toml
[registry]
min-publish-age = "14 days"
```

Accepted values: `"N days"` / `"N day"` / `"N weeks"` / `"N week"`, or a bare integer meaning days.

**Precedence (highest wins)**:

1. `--min-age <DAYS>` on the CLI
2. `[registry].min-publish-age` in `.cargo/config.toml` — searched from the manifest's directory up to the root, then `$CARGO_HOME/config.toml` (default `~/.cargo/config.toml`)

If neither is set, `cargo-aged` exits with an error rather than guessing a default — you have to opt in to a specific threshold. Use plain `cargo update` if you don't want any age filtering.

When the value comes from a config file, the effective threshold and its source are printed at the top of the run.

Once Cargo's `-Zmin-publish-age` is stable, the same config file will govern both the resolver and this tool.

### Options

| Flag | Default | Description |
| --- | --- | --- |
| `--min-age <DAYS>` | see below | Minimum release age in days before a crate is eligible for update. Overrides `.cargo/config.toml`. Required if no config file provides one — the tool exits with an error otherwise. |
| `--manifest-path <PATH>` | `./Cargo.toml` | Path to the `Cargo.toml` to read. |
| `--dry-run` | off | Print what would be updated without changing `Cargo.lock`. |
| `--verbose` | off | Also print the publish timestamp for each crate. |
| `--iterate` | off | Repeat passes until a full pass makes no changes (bounded at 10 passes). Useful for tightly-coupled dep families like `serde` + `serde_json` that can only be downgraded in stages. |
| `-h`, `--help` | | Print help. |
| `-V`, `--version` | | Print version. |

### What gets skipped

- **Path dependencies** (`path = "..."`) — nothing to fetch from crates.io.
- **Git dependencies** (`git = "..."`) — same.
- **`=`-pinned requirements** (`serde = "=1.0.210"`) — the pin is treated as an explicit choice and left alone.
- **Crates whose latest stable version is younger than `--min-age`.**
- **Crates already locked to an age-eligible version** — reported as `= serde 1.0.210 — already age-eligible` and left alone. Note this means `cargo-aged` won't proactively upgrade one age-eligible version to a newer age-eligible version within the same major; pair it with a plain `cargo update` first if you want to move forward before aging back.
- **Crates that 404 on crates.io or whose API call fails** — a warning is printed and the crate is skipped.

Yanked releases and pre-release versions (anything with a `-` suffix, e.g. `1.0.0-rc.1`) are ignored when picking the "latest stable" version.

### Example output

```
Checking 12 dependencies (min-age: 30 days)...
  ✓ serde 1.0.210            — 45 days old, updating...
  ✗ tokio 1.38.0             — 8 days old, skipping
  ✗ reqwest (git dep)        — skipping
  ✓ clap 4.5.4               — 62 days old, updating...
  ...
Summary: 3 updated, 9 skipped.
```

With `--dry-run`, the `updating...` lines change to `would update (dry-run)` and no `cargo update` is invoked.

## How it works

1. Parses your `Cargo.toml` (including `[dev-dependencies]`, `[build-dependencies]`, `[target.*.dependencies]`, and `[workspace.dependencies]`).
2. For each registry dependency, GETs `https://crates.io/api/v1/crates/<name>` with a descriptive `User-Agent` (per the crates.io fair-use policy).
3. Picks the newest version that is not yanked and not a pre-release.
4. Reads `Cargo.lock` and skips the crate if any age-eligible version is already locked (prevents redundant work and lets `--iterate` converge).
5. If `(now - published_at) >= min_age`, shells out to `cargo update -p <crate> --precise <version> --manifest-path <path>`.
6. If that `cargo update` fails (typically because another direct dep transitively constrains this crate to a newer range), retries with the next-older age-eligible version — up to 5 attempts per crate — and reports the successful pin, or the last error if all attempts fail.
7. With `--iterate`, repeats the whole pass until a pass produces zero updates (fixed point), bounded at 10 passes.
8. Prints a summary of updated vs skipped counts.

The age constraint is applied **only to the direct dependencies you declare in `Cargo.toml`**. Transitive deps are left to Cargo's normal resolver.

Your `Cargo.toml` is never modified — only `Cargo.lock` is touched, via `cargo update`.

## Development

```sh
cargo build
cargo run -- aged --dry-run                       # test against this repo's own manifest
cargo run -- aged --dry-run --manifest-path ../other-project/Cargo.toml
```

## License

MIT OR Apache-2.0
