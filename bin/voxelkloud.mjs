#!/usr/bin/env node
// The npm front door for a Rust binary.
//
// `npx voxelkloud@latest inspect <url>` has to work on a machine with no Rust
// toolchain, and it has to not download five platforms' worth of binaries to do
// it. The mechanism is npm's own: one binary per platform in its own package,
// each declaring `os` and `cpu`, all listed as OPTIONAL dependencies of this
// one. npm installs the single package that matches and silently skips the
// rest — the same shape esbuild and Biome use, chosen because it is the one
// every registry mirror, lockfile and offline cache already understands.
//
// This file only finds that binary and gets out of the way.

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { chmodSync, existsSync } from "node:fs";

const require = createRequire(import.meta.url);

/** npm's platform keys to the package that carries that build. */
const PACKAGES = {
  "darwin arm64": "@voxelkloud/cli-darwin-arm64",
  "darwin x64": "@voxelkloud/cli-darwin-x64",
  "linux arm64": "@voxelkloud/cli-linux-arm64",
  "linux x64": "@voxelkloud/cli-linux-x64",
  "win32 x64": "@voxelkloud/cli-win32-x64",
};

const key = `${process.platform} ${process.arch}`;
const pkg = PACKAGES[key];

if (pkg === undefined) {
  console.error(
    `voxelkloud: no prebuilt binary for ${key}.\n\n` +
      `  Build it yourself:  cargo install voxelkloud-cli\n` +
      `  Or open an issue:   https://github.com/voxelkloud/voxelkloud/issues\n\n` +
      `Supported: ${Object.keys(PACKAGES).join(", ")}.`,
  );
  process.exit(2);
}

const binary = process.platform === "win32" ? "voxelkloud.exe" : "voxelkloud";

let path;
try {
  // Resolved through the package's own entry rather than by walking
  // node_modules: that is what makes this work under pnpm's symlinked store,
  // Yarn PnP and a hoisted npm tree without three code paths.
  path = require.resolve(`${pkg}/bin/${binary}`);
} catch {
  console.error(
    `voxelkloud: the binary package ${pkg} is not installed.\n\n` +
      `This happens when the install ran with --no-optional, or when a lockfile\n` +
      `was written on a different platform and copied here.\n\n` +
      `  Fix:  npm install ${pkg}\n` +
      `  Or:   npm install voxelkloud --force\n`,
  );
  process.exit(2);
}

if (!existsSync(path)) {
  console.error(`voxelkloud: ${pkg} is installed but ${path} is missing.`);
  process.exit(2);
}

// Some registries and archive tools drop the executable bit. Restoring it is
// cheaper than the bug report that says "permission denied" on a fresh install.
if (process.platform !== "win32") {
  try {
    chmodSync(path, 0o755);
  } catch {
    // Read-only store (pnpm, Nix). If the bit is already right this is moot,
    // and if it is not, the spawn below fails with a message that says so.
  }
}

// `spawnSync` with inherited stdio rather than `exec`: `voxelkloud serve` runs
// until Ctrl-C and `inspect --json` is piped into `jq`, so the child needs the
// real terminal and the real pipes, not a buffer.
const result = spawnSync(path, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error(`voxelkloud: could not run ${path}: ${result.error.message}`);
  process.exit(2);
}
// A signal is not an exit code. Reporting 128+n is what a shell expects, and it
// is how `timeout` and CI runners tell a kill from a failure.
if (result.signal) {
  process.exit(128 + (require("node:os").constants.signals[result.signal] ?? 0));
}
process.exit(result.status ?? 0);
