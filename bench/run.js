// Compares unbinder with @apidevtools/json-schema-ref-parser on every
// document in the fixtures directory, and prints one table per document.

import { execFileSync } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import { cpus } from "node:os";
import { basename, join } from "node:path";

import $RefParser from "@apidevtools/json-schema-ref-parser";
import { Bench } from "tinybench";

import init, {
  dereferenceSchema,
  dereferenceSchemaText,
} from "../pkg/unbinder.js";
import { fixtures, root } from "./paths.js";

// The comparison is of speed, so unbinder's size limit is out of the way.
const maxBytes = 4_294_967_295;

const objectToObject = "object to object";
const textToText = "text to text";

/** The two ways the libraries are compared. */
const modes = [
  {
    name: "ignore",
    native: "ignore",
    note: "cycles are left as $ref, and both libraries return the same document",
    refParser: { dereference: { circular: "ignore" } },
    unbinder: { circular: "ignore", maxBytes },
    serializable: true,
  },
  {
    name: "default",
    native: "expand",
    note:
      "cycles are linked into the result, and both libraries return the same objects; a tree or text cannot hold a cycle, so those rows expand it one level",
    refParser: {},
    unbinder: { circular: "link", maxBytes },
    serializable: false,
  },
];

function median(samples) {
  const sorted = samples.toSorted((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
}

function mean(samples) {
  return samples.reduce((total, sample) => total + sample, 0) / samples.length;
}

/** One measurement, in milliseconds. */
function measurement(library, pipeline, samples, error) {
  return samples.length === 0
    ? { library, pipeline, error: error ?? "no samples" }
    : {
      library,
      pipeline,
      median: median(samples),
      mean: mean(samples),
      fastest: samples.reduce((least, sample) => Math.min(least, sample)),
      samples: samples.length,
    };
}

/** Times the JavaScript libraries on one document in one mode. */
async function measureJavaScript(text, parsed, mode) {
  const bench = new Bench({
    time: 3_000,
    iterations: 5,
    warmupTime: 1_000,
    warmupIterations: 2,
    retainSamples: true,
  });
  // ref-parser rewrites the document it is given, so each call gets a copy
  // made outside the measurement.
  let copy;
  const collect = () => globalThis.gc?.();
  bench.add(
    `ref-parser|${objectToObject}`,
    () => $RefParser.dereference(copy, mode.refParser),
    {
      beforeEach: () => {
        copy = structuredClone(parsed);
        collect();
      },
    },
  );
  bench.add(
    `unbinder (wasm)|${objectToObject}`,
    () => dereferenceSchema(parsed, mode.unbinder).schema,
    { beforeEach: collect },
  );
  if (mode.serializable) {
    bench.add(
      `ref-parser|${textToText}`,
      async () =>
        JSON.stringify(
          await $RefParser.dereference(JSON.parse(text), mode.refParser),
        ),
      { beforeEach: collect },
    );
  }
  bench.add(
    `unbinder (wasm)|${textToText}`,
    () => dereferenceSchemaText(text, mode.unbinder),
    { beforeEach: collect },
  );
  await bench.run();
  return bench.tasks.map((task) => {
    const [library, pipeline] = task.name.split("|");
    return task.result.state === "completed"
      ? measurement(library, pipeline, task.result.latency.samples)
      : measurement(
        library,
        pipeline,
        [],
        task.result.error?.message ?? task.result.state,
      );
  });
}

/** Runs the native benchmark, which times every document in one go. */
function measureNative() {
  const output = execFileSync("cargo", ["bench", "--bench", "native"], {
    cwd: root,
    env: { ...process.env, UNBINDER_BENCH_FIXTURES: fixtures },
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    stdio: ["ignore", "pipe", "inherit"],
  });
  return output.trim().split("\n").map((line) => JSON.parse(line));
}

/**
 * Whether two results hold the same values. Either may hold one object in
 * many places and may hold cycles, so a pair of objects is compared once,
 * however many places hold it. The work is then in proportion to the objects
 * and not to the tree they would write out as.
 */
function identical(theirs, ours) {
  const compared = new Map();
  const pending = [[theirs, ours]];
  while (pending.length > 0) {
    const [left, right] = pending.pop();
    if (
      typeof left !== "object" || left === null ||
      typeof right !== "object" || right === null
    ) {
      if (!Object.is(left, right)) {
        return false;
      }
      continue;
    }
    const partners = compared.get(left) ?? new Set();
    if (partners.has(right)) {
      continue;
    }
    compared.set(left, partners.add(right));
    const names = Object.keys(left);
    if (
      Array.isArray(left) !== Array.isArray(right) ||
      names.length !== Object.keys(right).length
    ) {
      return false;
    }
    for (const name of names) {
      if (!Object.hasOwn(right, name)) {
        return false;
      }
      pending.push([left[name], right[name]]);
    }
  }
  return true;
}

function milliseconds(value) {
  return value >= 100 ? value.toFixed(0) : value.toPrecision(3);
}

/** How a measurement compares with ref-parser doing the same job. */
function comparison(row, rows) {
  // The native library has no objects to convert, so its values are set
  // against ref-parser's objects.
  const pipeline = row.pipeline.startsWith("value to ")
    ? objectToObject
    : row.pipeline;
  const baseline = rows.find((other) =>
    other.library === "ref-parser" && other.pipeline === pipeline
  );
  if (row.median === undefined || baseline?.median === undefined) {
    return "";
  }
  if (row === baseline) {
    return "baseline";
  }
  const ratio = baseline.median / row.median;
  return ratio >= 1
    ? `${ratio.toFixed(1)}x faster`
    : `${(1 / ratio).toFixed(1)}x slower`;
}

function table(rows) {
  return rows.map((row) =>
    row.median === undefined
      ? { library: row.library, pipeline: row.pipeline, result: row.error }
      : {
        library: row.library,
        pipeline: row.pipeline,
        "median ms": milliseconds(row.median),
        "mean ms": milliseconds(row.mean),
        "fastest ms": milliseconds(row.fastest),
        samples: row.samples,
        "vs ref-parser": comparison(row, rows),
      }
  );
}

/** The version in the package manifest at `location`. */
async function version(location) {
  const manifest = await readFile(new URL(location, import.meta.url), "utf8");
  return JSON.parse(manifest).version;
}

await init({
  module_or_path: await readFile(
    new URL("../pkg/unbinder_bg.wasm", import.meta.url),
  ),
});

console.log(`node ${process.version} on ${cpus()[0].model}`);
console.log(
  `@apidevtools/json-schema-ref-parser ${await version(
    "node_modules/@apidevtools/json-schema-ref-parser/package.json",
  )}`,
);
console.log(`unbinder ${await version("../pkg/package.json")}`);

const native = measureNative();
const names = (await readdir(fixtures))
  .filter((name) => name.endsWith(".json"))
  .toSorted();

for (const name of names) {
  const fixture = basename(name, ".json");
  const text = await readFile(join(fixtures, name), "utf8");
  const parsed = JSON.parse(text);
  console.log(`\n# ${fixture} (${(text.length / 1024).toFixed(0)} KiB)`);

  for (const mode of modes) {
    console.log(`\n${mode.name}: ${mode.note}`);
    const theirs = await $RefParser.dereference(
      structuredClone(parsed),
      mode.refParser,
    );
    const ours = dereferenceSchema(parsed, mode.unbinder).schema;
    console.log(`outputs identical: ${identical(theirs, ours)}`);
    const rows = [
      ...await measureJavaScript(text, parsed, mode),
      ...native
        .filter((entry) =>
          entry.fixture === fixture && entry.mode === mode.native
        )
        .map((entry) =>
          measurement(
            "unbinder (native)",
            entry.task,
            entry.nanoseconds.map((sample) => sample / 1_000_000),
            entry.error,
          )
        ),
    ];
    console.table(table(rows));
  }
}
