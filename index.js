// This package exists to reserve the name and to point you at the real one.
// It has no implementation on purpose — see README.md.
throw new Error(
  [
    "voxelkloud ships as scoped packages; there is no `voxelkloud` module to import.",
    "",
    "  React        npm install @voxelkloud/react three",
    "  Vue 3        npm install @voxelkloud/vue three",
    "  No framework npm install @voxelkloud/view @voxelkloud/loader three",
    "  Data only    npm install @voxelkloud/loader",
    "",
    "https://github.com/voxelkloud/voxelkloud",
  ].join("\n"),
);
