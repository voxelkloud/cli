# voxelkloud

The command-line tool. Also the signpost to the browser packages.

```sh
npx voxelkloud@latest inspect https://example.com/clouds/autzen/ --deep
```

No toolchain, no build step: this package carries a small launcher, and npm
installs the one prebuilt binary that matches your platform.

## What it does

```sh
voxelkloud inspect <target>    # what is this? format, size, attributes, CRS, hierarchy
voxelkloud doctor  <url>       # why does this deployment feel broken?
voxelkloud serve   [dir]       # a static server with byte ranges and CORS
voxelkloud convert <inputs...> # LAS/LAZ in; Potree v2, COPC or EPT out
```

`inspect` reads a directory, a manifest, a `.las`/`.laz`/`.copc.laz` or a URL,
and prints the same shape for all of them.

`doctor` is the one to reach for when a cloud loads locally and not in
production. It asks the questions the formats actually depend on — does the
server answer `206`, does the browser get to read `Content-Range`, is something
gzipping a file that is already compressed — and says what to change. It works
against a Potree deployment you already have, and exits non-zero when something
is broken, so it fits in CI:

```yaml
- uses: voxelkloud/voxelkloud@v1
  with:
    run: doctor https://cdn.example.com/clouds/site-a/ --deep
```

## Other ways to install

```sh
npm install -g voxelkloud
cargo install voxelkloud-cli
brew install voxelkloud/tap/voxelkloud
```

Or a static binary from the [releases](https://github.com/voxelkloud/voxelkloud/releases).

## The browser packages

`import "voxelkloud"` throws on purpose — there is no module here. The renderer
ships as scoped packages, so you take only the part you need.

| You want | Install |
| --- | --- |
| A React component | `npm install @voxelkloud/react three` |
| A Vue 3 component | `npm install @voxelkloud/vue three` |
| The renderer, no framework | `npm install @voxelkloud/view @voxelkloud/loader three` |
| To read cloud data, not draw it | `npm install @voxelkloud/loader` |

```tsx
import { PointCloudViewer } from "@voxelkloud/react";

<PointCloudViewer
  url="https://example.com/clouds/autzen/"
  style={{ position: "absolute", inset: 0 }}
/>;
```

Full documentation: <https://github.com/voxelkloud/voxelkloud>

MIT.
