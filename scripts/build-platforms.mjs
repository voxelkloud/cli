// Assemble the per-platform npm packages from built binaries.
//
// Each is a package with one file in it and no code: the `os`/`cpu` fields are
// the whole mechanism, because they are what makes npm install the one that
// matches and skip the other four. The launcher in `bin/` then resolves it.
//
// Run after cross-compiling, with the binaries laid out as
//   <artifacts>/<rust target triple>/voxelkloud[.exe]
// which is exactly what the release workflow's matrix uploads.
//
//   node scripts/build-platforms.mjs --artifacts ./artifacts --out ./npm
//
// Then `npm publish` each directory under `--out`, and the launcher package
// last: publishing the launcher first leaves a window where `npx voxelkloud`
// resolves to a package whose optional dependencies do not exist yet.

import { chmodSync, copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const LAUNCHER = JSON.parse(readFileSync(join(HERE, "..", "package.json"), "utf8"));

/** npm platform, npm arch, and the Rust target that produces it. */
const PLATFORMS = [
  { npm: "darwin-arm64", os: "darwin", cpu: "arm64", target: "aarch64-apple-darwin" },
  { npm: "darwin-x64", os: "darwin", cpu: "x64", target: "x86_64-apple-darwin" },
  // musl, statically linked, for both Linux flavours. One artefact that runs on
  // Debian and on Alpine beats two that each run on one, and npm has no
  // reliable way to select on libc anyway.
  { npm: "linux-arm64", os: "linux", cpu: "arm64", target: "aarch64-unknown-linux-musl" },
  { npm: "linux-x64", os: "linux", cpu: "x64", target: "x86_64-unknown-linux-musl" },
  { npm: "win32-x64", os: "win32", cpu: "x64", target: "x86_64-pc-windows-msvc" },
];

function arg(name, fallback) {
  const at = process.argv.indexOf(`--${name}`);
  return at >= 0 ? process.argv[at + 1] : fallback;
}

const artifacts = resolve(arg("artifacts", "artifacts"));
const outRoot = resolve(arg("out", "npm"));
const version = arg("version", LAUNCHER.version);

rmSync(outRoot, { recursive: true, force: true });

const built = [];
for (const platform of PLATFORMS) {
  const exe = platform.os === "win32" ? "voxelkloud.exe" : "voxelkloud";
  const source = join(artifacts, platform.target, exe);
  try {
    const dir = join(outRoot, `cli-${platform.npm}`);
    mkdirSync(join(dir, "bin"), { recursive: true });
    copyFileSync(source, join(dir, "bin", exe));
    if (platform.os !== "win32") chmodSync(join(dir, "bin", exe), 0o755);

    writeFileSync(
      join(dir, "package.json"),
      `${JSON.stringify(
        {
          name: `@voxelkloud/cli-${platform.npm}`,
          version,
          description: `The voxelkloud CLI binary for ${platform.os} ${platform.cpu}.`,
          license: LAUNCHER.license,
          repository: LAUNCHER.repository,
          homepage: LAUNCHER.homepage,
          bugs: LAUNCHER.bugs,
          publishConfig: { access: "public" },
          // The two fields that do the work.
          os: [platform.os],
          cpu: [platform.cpu],
          files: ["bin"],
          // Deliberately no `bin`: the launcher package owns the command name,
          // and a second one here would race it for the same shim.
        },
        null,
        2,
      )}\n`,
    );
    writeFileSync(
      join(dir, "README.md"),
      `# @voxelkloud/cli-${platform.npm}\n\n` +
        `The \`voxelkloud\` binary for ${platform.os} ${platform.cpu}. Installed\n` +
        `automatically as an optional dependency of \`voxelkloud\`; there is no\n` +
        `reason to depend on it directly.\n\n` +
        `    npx voxelkloud@latest inspect <url>\n\n` +
        `MIT. https://github.com/voxelkloud/voxelkloud\n`,
    );
    copyFileSync(join(HERE, "..", "LICENSE"), join(dir, "LICENSE"));
    built.push(platform.npm);
  } catch (error) {
    console.warn(`skipping ${platform.npm}: ${error.message}`);
  }
}

if (built.length === 0) {
  console.error(`No binaries found under ${artifacts}. Nothing to publish.`);
  process.exit(1);
}

// The launcher's optional dependencies must name exactly the packages that were
// built, at exactly this version. Writing them here rather than by hand is what
// stops a release from listing a platform it did not produce.
const launcherOut = join(outRoot, "voxelkloud");
mkdirSync(join(launcherOut, "bin"), { recursive: true });
copyFileSync(join(HERE, "..", "bin", "voxelkloud.mjs"), join(launcherOut, "bin", "voxelkloud.mjs"));
chmodSync(join(launcherOut, "bin", "voxelkloud.mjs"), 0o755);
copyFileSync(join(HERE, "..", "index.js"), join(launcherOut, "index.js"));
copyFileSync(join(HERE, "..", "README.md"), join(launcherOut, "README.md"));
copyFileSync(join(HERE, "..", "LICENSE"), join(launcherOut, "LICENSE"));
writeFileSync(
  join(launcherOut, "package.json"),
  `${JSON.stringify(
    {
      ...LAUNCHER,
      version,
      scripts: undefined,
      optionalDependencies: Object.fromEntries(
        built.map((npm) => [`@voxelkloud/cli-${npm}`, version]),
      ),
    },
    null,
    2,
  )}\n`,
);

console.log(`Built ${built.length} platform package(s) plus the launcher in ${outRoot}:`);
for (const npm of built) console.log(`  @voxelkloud/cli-${npm}@${version}`);
console.log(`  voxelkloud@${version}`);
