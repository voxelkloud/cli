# voxelkloud-cli

The `voxelkloud` command: inspect a cloud, diagnose a deployment, serve one
locally, convert between formats.

```sh
npx voxelkloud@latest inspect https://example.com/clouds/autzen/ --deep
cargo install voxelkloud-cli
```

The crate is named for what it is; the binary is named for what you type.

## The commands

**`inspect <target>`** — what is this? Format, point count, extent, attributes,
projection, and with `--deep` the whole hierarchy: how many nodes, how deep, how
many points per level. Reads a directory, a manifest, a `.las`/`.laz`/`.copc.laz`
or an `http(s)` URL, and the output shape is the same for all of them. `--json`
for a machine.

**`doctor <url>`** — why does this deployment feel broken? Byte ranges, CORS,
transport compression, cache policy, index shape. Every streaming format rests
on a small set of HTTP behaviours, and when one is missing the failure surfaces
as something else entirely — no `206` looks like a corrupt file, a missing
`Access-Control-Expose-Headers` looks like a truncated read. It grades a Potree
deployment as readily as one of ours, and exits non-zero when something is
broken, so a CI job can gate on it.

**`serve [dir]`** — a static server that gets byte ranges, CORS and *not*
re-compressing already-compressed payloads right. Not a production server.

**`convert <inputs...>`** — LAS, LAZ, COPC and E57 in; COPC, Potree v2
(`DEFAULT` or `BROTLI`) or EPT out. Several inputs become one cloud, which is what a survey
that ships as four hundred tiles needs. Out of core above a memory budget, so
the ceiling is the disk's rather than the machine's.

```sh
voxelkloud convert survey/*.laz -o survey.copc.laz
voxelkloud convert scan.las -o out/ --format potree-brotli
voxelkloud convert station.e57 -o station.copc.laz
```

An E57 is read scan by scan with each scan's pose applied, spherical
coordinates converted, and the records that carry no position — the
no-returns of a scan grid — dropped rather than written at the origin. Which
scan a point came from survives as its LAS point source id. Converting one
reads it twice: a posed scan's declared bounds are not the box its points
occupy, and the header this writes has to be measured rather than copied.

**`optimize <dir>`** — the same cloud, better bytes. Re-encodes a Potree v2
cloud without rebuilding its tree: every node keeps its key, its count and its
place, and only the payloads change. `--encoding brotli` is about a third of the
size on the wire; `--drop gps-time` removes a field nothing reads. Converting
would produce a *different* tree and invalidate every cache and every URL
anybody wrote down.

```sh
voxelkloud optimize clouds/autzen -o clouds/autzen-small --encoding brotli
# 356 MiB -> 125 MiB, 4,377 nodes, 10,653,336 points, unchanged tree
```

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Ran, and found nothing wrong |
| 1 | Ran, and found something wrong — a `doctor` finding at `fail` |
| 2 | Could not run: bad arguments, unreachable target, unreadable file |

MIT.
