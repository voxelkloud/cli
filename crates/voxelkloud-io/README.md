# voxelkloud-io

Reading and writing point clouds: Potree v2, COPC, EPT and LAS/LAZ, through one
neutral vocabulary.

```rust
use voxelkloud_io::format::open_path;

let cloud = open_path(std::path::Path::new("clouds/autzen"))?;
let info = cloud.info();
println!("{} — {} points", info.format.title(), info.point_count);

let stats = cloud.hierarchy()?;
println!("{} nodes, depth {}", stats.nodes, stats.depth);
```

The library half of the [voxelkloud](https://github.com/voxelkloud/voxelkloud)
toolchain, and the native twin of `@voxelkloud/core` plus its drivers. What the
browser streams, this produces.

## Three rules

They are carried over from the TypeScript because they are what made it work,
not for symmetry.

**Nothing here opens a socket.** Bytes enter through `ByteSource`, and a reader
says which ranges of which relative path it wants. That is what lets one code
path serve a local directory, an HTTP deployment and — later — a directory
handle in a browser tab. It is also why `voxelkloud doctor` can grade a
transport the reader is deliberately unable to see.

**Anomalies are tolerated, not thrown.** Real files break their own specs: a
`uint8` attribute with a negative minimum, an `elementSize` that contradicts its
own type, an Extra Bytes VLR that describes more bytes than the record has. A
reader that refused them reads almost nothing; one that ignored them lies. They
land in `warnings` on the value, in discovery order.

**The vocabulary names no format.** A `CloudInfo` from a Potree directory and
one from a COPC file are the same type, and code that wants the difference has
to ask for it.

## Features

| Feature | Default | What it costs |
| --- | --- | --- |
| `formats` | on | serde + serde_json, for the Potree and EPT manifests |
| `laz` | on | laz-rs, for compressed point records |

With `default-features = false` only the `las` module compiles, and it has no
dependencies at all — which is the configuration `@voxelkloud/wasm-codecs`
builds against.

MIT.
