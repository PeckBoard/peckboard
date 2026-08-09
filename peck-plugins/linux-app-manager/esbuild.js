const esbuild = require("esbuild");

esbuild
  .build({
    entryPoints: ["src/index.ts"],
    outfile: "dist/index.js",
    bundle: true,
    sourcemap: false,
    minify: false,
    format: "cjs",
    target: ["es2020"],
    logLevel: "info",
  })
  .catch(() => process.exit(1));
