# crates

The native toolchain: a library and the CLI built on it.

| Crate | What it is |
| --- | --- |
| `voxelkloud-io` | Reading and writing Potree v2, COPC, EPT and LAS/LAZ, through one neutral vocabulary. The Rust twin of `@voxelkloud/core` and its drivers |
| `voxelkloud-cli` | The `voxelkloud` command: `inspect`, `doctor`, `serve`, `convert` |

A workspace of its own rather than a manifest at the repo root, because the
three Rust crates under `packages/` are npm packages that happen to be written
in Rust — built by `pnpm --filter`, with their own lockfiles, compiled to wasm.
These two are the opposite: Rust first, with npm as one of several ways they
ship.

```sh
cd crates
cargo test                       # unit tests, plus the real-file suite when demo/data is present
cargo build --release            # target/release/voxelkloud
cargo run -- inspect ../demo/data/real --deep
```

`tests/oracle.rs` is the one worth knowing about: it converts the file
PotreeConverter converted and compares the two manifests. Byte-for-byte is not
the bar — two converters sampling a cloud pick different representative points —
but the quantum, the origin, the cube, the spacing and the whole attribute list
are, and they match.

To check the output against the reader that will actually load it:

```sh
cargo run --release -- serve ../demo/data --port 8080
node ../demo/verify-conversion.mjs http://127.0.0.1:8080/my-cloud/
```

A Rust writer and a TypeScript reader that share no code, which is what makes
agreement between them evidence rather than tautology.

The real-file tests read `demo/data` and `demo/potree/pointclouds`, which are
gitignored. They skip with a note when the datasets are not on the machine, so a
fresh clone tests green without downloading 5 GB of LiDAR.

## Why the library is not the wasm codecs

`@voxelkloud/wasm-codecs` decodes LAZ in a browser. `voxelkloud-io` reads whole
formats on a machine with a filesystem. They share the LAS framing — one
module, one truth — and nothing else: the codec package is 148 KB of wasm whose
job is to be small, and this one links serde and writes files.

## Distribution

Five ways in, all of the same binary:

```sh
npx voxelkloud@latest inspect <url>   # no toolchain, no install
npm install -g voxelkloud
cargo install voxelkloud-cli
brew install voxelkloud/tap/voxelkloud
curl -L .../voxelkloud-<version>-<target>.tar.gz | tar xz
```

Plus a GitHub Action (`voxelkloud/voxelkloud@v1`) for putting `doctor` in the
pipeline that does the deploying.

`.github/workflows/release-cli.yml` builds the five targets and publishes them.
See the header of `packages/voxelkloud/scripts/build-platforms.mjs` for how the
npm side fits together, and why the launcher is published last.
