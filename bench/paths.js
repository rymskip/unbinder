// Where the benchmark keeps things.

import { fileURLToPath } from "node:url";

/** The repository root. */
export const root = fileURLToPath(new URL("..", import.meta.url));

/** The directory of documents the benchmark runs on. */
export const fixtures = process.env.UNBINDER_BENCH_FIXTURES ??
  fileURLToPath(new URL("fixtures", import.meta.url));
