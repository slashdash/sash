# Contributing to Sash

Thanks for helping. Small, focused changes are easiest to review.

## Before you start

- For a bug, open an issue with the build number from **Settings**, your macOS
  version, and the relevant lines from `~/Library/Logs/Sash/sash.log`.
- For a feature with a real trade-off (a new dependency, a layout model, a change
  to what Sash owns versus libghostty), open an issue first. Decisions that stick
  are recorded in [`docs/adr/`](docs/adr/).

## Setup

```sh
git clone https://github.com/slashdash/sash.git
cd sash
scripts/fetch-libghostty.sh
cargo run --release
```

macOS 12+, Rust 1.85+ (edition 2024), and the Xcode command-line tools are
required. Rune is pinned to a `dev` revision and the first build takes a few
minutes.

## Checks

Every pull request should pass:

```sh
cargo fmt --check
cargo clippy --all-targets
cargo test
```

If you change dependency licences or add a crate, regenerate the notices:

```sh
cargo about generate about.hbs -o THIRD-PARTY.html
```

## Style

- Match the surrounding code; comments explain *why*, not *what*.
- Keep terminal state in libghostty-vt. Sash must not grow a shadow grid,
  scrollback index, title parser or key encoder.
- UI changes should come with a headless snapshot (`SASH_DEMO=1 ... --snap`)
  in the pull request description.
- One logical change per commit, single-line conventional subject
  (`fix: keep the strip visible with one pinned tab`).

## Licence

Sash is MIT licensed. By contributing you agree that your contribution is
licensed under the same terms; no CLA is required. Add no code you cannot
license that way.
