// Builds the package that is published to npm and JSR into pkg/.

import { execFileSync } from "node:child_process";
import { readFileSync, rmSync, writeFileSync } from "node:fs";

const jsrScope = "@rymskip";
const root = new URL("../", import.meta.url);
const pkg = new URL("pkg/", root);

execFileSync(
  "wasm-pack",
  ["build", "--target", "web", "--", "--features", "wasm"],
  { cwd: root, stdio: "inherit" },
);

// wasm-pack leaves the JavaScript the module imports out of the files it
// lists, and a package published without it cannot load.
const manifest = JSON.parse(readFileSync(new URL("package.json", pkg), "utf8"));
manifest.files.push("snippets");
writeFileSync(
  new URL("package.json", pkg),
  `${JSON.stringify(manifest, null, 2)}\n`,
);

// The JSR manifest is derived from the npm one, which wasm-pack derives from
// Cargo.toml, so the version is only ever written there.
writeFileSync(
  new URL("jsr.json", pkg),
  `${
    JSON.stringify(
      {
        name: `${jsrScope}/${manifest.name}`,
        version: manifest.version,
        license: manifest.license,
        exports: `./${manifest.main}`,
        publish: { include: [...manifest.files, "README.md", "LICENSE"] },
      },
      null,
      2,
    )
  }\n`,
);

// wasm-pack ignores everything in pkg/, which hides it from `jsr publish`.
rmSync(new URL(".gitignore", pkg));
