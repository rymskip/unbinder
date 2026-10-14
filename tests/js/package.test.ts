import assert from "node:assert/strict";
import dns from "node:dns/promises";
import { mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import https from "node:https";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { Readable } from "node:stream";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import init, {
  type LoadOptions,
  dereferenceSchema,
  dereferenceSchemaFrom,
  dereferenceSchemaText,
  schemaStats,
} from "../../pkg/unbinder.js";

await init({
  module_or_path: await readFile(
    new URL("../../pkg/unbinder_bg.wasm", import.meta.url),
  ),
});

const recursive = {
  definitions: {
    node: { properties: { next: { $ref: "#/definitions/node" } } },
  },
  list: { $ref: "#/definitions/node" },
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function throwsError(action: () => unknown, fragment: string): void {
  assert.throws(action, (error: unknown) => {
    assert.ok(error instanceof Error);
    assert.ok(error.message.includes(fragment), error.message);
    return true;
  });
}

test("inlines references into plain objects", () => {
  const { schema, circular } = dereferenceSchema({
    properties: {
      name: { $ref: "#/definitions/name" },
      nickname: { default: null },
    },
    definitions: { name: { type: "string", minLength: 1 } },
  });

  assert.deepEqual(schema, {
    properties: {
      name: { type: "string", minLength: 1 },
      nickname: { default: null },
    },
    definitions: { name: { type: "string", minLength: 1 } },
  });
  assert.deepEqual(circular, []);
});

test("links a recursive schema back into itself", () => {
  const { schema, circular } = dereferenceSchema(recursive);

  assert.ok(isRecord(schema) && isRecord(schema.definitions));
  const node = schema.definitions.node;
  assert.ok(isRecord(node) && isRecord(node.properties));
  assert.equal(schema.list, node);
  assert.equal(node.properties.next, node);
  assert.deepEqual(circular, ["#/definitions/node"]);
  assert.throws(() => JSON.stringify(schema), TypeError);
});

test("links a cycle that runs through several definitions", () => {
  const { schema } = dereferenceSchema({
    a: { b: { $ref: "#/b" } },
    b: { c: [{ $ref: "#/c" }] },
    c: { a: { $ref: "#/a" }, title: "C" },
    entry: { $ref: "#/b" },
  });

  assert.ok(isRecord(schema) && isRecord(schema.a) && isRecord(schema.b));
  assert.ok(isRecord(schema.c) && Array.isArray(schema.b.c));
  assert.equal(schema.a.b, schema.b);
  assert.equal(schema.b.c[0], schema.c);
  assert.equal(schema.c.a, schema.a);
  assert.equal(schema.entry, schema.b);
  assert.equal(schema.c.title, "C");
});

test("links a cycle far longer than the depth limit", () => {
  const length = 20_000;
  const ring: Record<string, unknown> = {};
  for (let index = 0; index < length; index += 1) {
    ring[`d${index}`] = { next: [{ $ref: `#/d${(index + 1) % length}` }] };
  }

  const { schema } = dereferenceSchema(ring);

  assert.ok(isRecord(schema));
  let node = schema.d0;
  for (let index = 1; index <= length; index += 1) {
    assert.ok(isRecord(node) && Array.isArray(node.next));
    node = node.next[0];
    assert.equal(node, schema[`d${index % length}`]);
  }
});

test("links a long chain of references that each add a keyword", () => {
  const length = 100;
  const chain: Record<string, unknown> = { d0: { type: "string" } };
  for (let index = 1; index < length; index += 1) {
    chain[`d${index}`] = { $ref: `#/d${index - 1}`, [`k${index}`]: index };
  }

  const { schema } = dereferenceSchema(chain);

  assert.ok(isRecord(schema) && isRecord(schema.d99));
  assert.equal(Object.keys(schema.d99).length, length);
  assert.equal(schema.d99.type, "string");
  assert.equal(schema.d99.k1, 1);
});

test("a chain of bare references past the depth limit is an error", () => {
  const chain: Record<string, unknown> = { d0: { type: "string" } };
  for (let index = 1; index <= 200; index += 1) {
    chain[`d${index}`] = { $ref: `#/d${index - 1}` };
  }
  chain.last = { $ref: "#/d200" };

  for (const circular of ["link", "expand"] as const) {
    throwsError(
      () => dereferenceSchema(chain, { circular }),
      "depth limit of 128 levels exceeded",
    );
  }
});

test("merging the members of two cycles keeps the links of both", () => {
  const input = {
    a: { properties: { b: { $ref: "#/b" }, own: { type: "string" } } },
    b: { properties: { a: { $ref: "#/a" }, own: { type: "number" } } },
    merged: { $ref: "#/a", properties: { $ref: "#/b/properties" } },
  };

  const { schema } = dereferenceSchema(input);
  const expanded = dereferenceSchema(input, { circular: "expand" }).schema;

  assert.ok(isRecord(schema) && isRecord(schema.merged));
  assert.ok(isRecord(expanded) && isRecord(expanded.merged));
  const properties = schema.merged.properties;
  assert.ok(isRecord(properties));
  assert.deepEqual(Object.keys(properties).toSorted(), ["a", "b", "own"]);
  assert.deepEqual(properties.own, { type: "number" });
  assert.equal(properties.b, schema.b);
  assert.equal(properties.a, schema.a);
  assert.ok(isRecord(expanded.merged.properties));
  assert.deepEqual(
    Object.keys(expanded.merged.properties).toSorted(),
    ["a", "b", "own"],
  );
});

test("a keyword merges into a link to a recursive target", () => {
  const { schema } = dereferenceSchema({
    node: { properties: { child: { $ref: "#/node" } } },
    labelled: {
      $ref: "#/node",
      properties: { child: { description: "the first child" } },
    },
  });

  assert.ok(isRecord(schema) && isRecord(schema.labelled));
  assert.ok(isRecord(schema.node) && isRecord(schema.labelled.properties));
  const child = schema.labelled.properties.child;
  assert.ok(isRecord(child) && isRecord(child.properties));
  assert.equal(child.description, "the first child");
  assert.equal(child.properties.child, schema.node);
  assert.equal("description" in schema.node, false);
});

test("a keyword does not replace a link that cannot be merged into yet", () => {
  const { schema } = dereferenceSchema({
    outer: {
      inner: { $ref: "#/other", properties: { back: { title: "Back" } } },
    },
    other: { properties: { back: { $ref: "#/outer" } } },
  });

  assert.ok(isRecord(schema) && isRecord(schema.outer));
  assert.ok(isRecord(schema.outer.inner));
  assert.ok(isRecord(schema.outer.inner.properties));
  assert.deepEqual(schema.outer.inner.properties.back, {
    title: "Back",
    $ref: "#/outer",
  });
});

test("two cycles that would merge forever hit the depth limit", () => {
  throwsError(
    () =>
      dereferenceSchema({
        a: { properties: { next: { $ref: "#/a" } } },
        b: { properties: { next: { $ref: "#/b" } } },
        both: { $ref: "#/a", properties: { $ref: "#/b/properties" } },
      }),
    "depth limit of 128 levels exceeded",
  );
});

// Linking must not change what a schema allows, whatever it links through.
test("linking keeps every keyword that expanding keeps", () => {
  const input = {
    real: { type: "string", properties: { $ref: "#/props" } },
    props: { id: { type: "integer" } },
    alias: { $ref: "#/real" },
    limited: { $ref: "#/alias", maxLength: 5 },
    named: { $ref: "#/real", properties: { name: { type: "string" } } },
    extended: { $ref: "#/real", properties: { $ref: "#/more" } },
    more: { extra: { type: "boolean" } },
    anything: true,
    bounded: { $ref: "#/anything", maxLength: 5 },
  };

  const linked = dereferenceSchema(input).schema;
  const expanded = dereferenceSchema(input, { circular: "expand" }).schema;

  assert.deepEqual(linked, expanded);
  assert.ok(isRecord(linked));
  assert.deepEqual(linked.limited, {
    maxLength: 5,
    type: "string",
    properties: { id: { type: "integer" } },
  });
  assert.deepEqual(linked.named, {
    type: "string",
    properties: { id: { type: "integer" }, name: { type: "string" } },
  });
  assert.deepEqual(linked.extended, {
    type: "string",
    properties: { id: { type: "integer" }, extra: { type: "boolean" } },
  });
  assert.deepEqual(linked.bounded, { maxLength: 5 });
});

test("keeps the members of an object in the order they were written", () => {
  const { schema } = dereferenceSchema({
    zebra: { $ref: "#/base", title: "Zebra", apple: 1 },
    base: { middle: true, alpha: true, title: "Base" },
    apple: { $ref: "#/base" },
  });

  assert.ok(isRecord(schema) && isRecord(schema.zebra));
  assert.deepEqual(Object.keys(schema), ["zebra", "base", "apple"]);
  assert.deepEqual(Object.keys(schema.zebra), [
    "title",
    "apple",
    "middle",
    "alpha",
  ]);
});

test("expands a recursive schema one level and reports the cycle", () => {
  const { schema, circular } = dereferenceSchema(recursive, {
    circular: "expand",
  });

  assert.deepEqual(schema, {
    definitions: {
      node: {
        properties: {
          next: { properties: { next: { $ref: "#/definitions/node" } } },
        },
      },
    },
    list: { properties: { next: { $ref: "#/definitions/node" } } },
  });
  assert.deepEqual(circular, ["#/definitions/node"]);
});

test("rejects a cycle on request", () => {
  throwsError(
    () => dereferenceSchema(recursive, { circular: "reject" }),
    "reference `#/definitions/node` is circular",
  );
});

test("enforces the limits it is given", () => {
  throwsError(
    () => dereferenceSchemaText("[1, 2]", { maxBytes: 8 }),
    "output takes more than 8 bytes",
  );
  throwsError(
    () => dereferenceSchema([[[1]]], { maxDepth: 3 }),
    "depth limit of 3 levels exceeded",
  );
});

test("every reference to a target shares one object", () => {
  const input = {
    definitions: { name: { type: "string" } },
    first: { $ref: "#/definitions/name" },
    second: { $ref: "#/definitions/name" },
    titled: { $ref: "#/definitions/name", title: "Titled" },
  };

  const { schema } = dereferenceSchema(input);

  assert.ok(isRecord(schema) && isRecord(schema.definitions));
  assert.equal(schema.first, schema.second);
  assert.equal(schema.first, schema.definitions.name);
  assert.deepEqual(schema.titled, { type: "string", title: "Titled" });
  assert.notEqual(schema.titled, schema.first);
  assert.deepEqual(input.first, { $ref: "#/definitions/name" });
});

test("returns a result too large to write out", () => {
  const doubling: Record<string, unknown> = { level40: "leaf" };
  for (let level = 0; level < 40; level += 1) {
    const next = { $ref: `#/level${level + 1}` };
    doubling[`level${level}`] = [next, next];
  }

  const { schema } = dereferenceSchema(doubling);

  assert.ok(isRecord(schema) && Array.isArray(schema.level0));
  assert.equal(schema.level0[0], schema.level0[1]);
  assert.equal(schema.level0[0], schema.level1);
  throwsError(
    () => dereferenceSchemaText(JSON.stringify(doubling)),
    "output takes more than",
  );
});

test("bounds what the shared objects take", () => {
  const wide: Record<string, unknown> = {};
  for (let member = 0; member < 2_000; member += 1) {
    wide[`member${member}`] = { type: "string" };
  }
  const input: Record<string, unknown> = { wide };
  for (let site = 0; site < 2_000; site += 1) {
    input[`site${site}`] = { $ref: "#/wide", title: `Site ${site}` };
  }

  // Each site is an object of its own with all two thousand members.
  throwsError(
    () => dereferenceSchema(input, { maxBytes: 1_000_000 }),
    "output takes more than 1000000 bytes",
  );
  const { schema } = dereferenceSchema(input);
  assert.ok(isRecord(schema) && isRecord(schema.site0) && isRecord(schema.wide));
  assert.equal(Object.keys(schema.site0).length, 2_001);
  assert.equal(schema.site0.member0, schema.wide.member0);
});

test("links a member named __proto__ as a plain member", () => {
  const input = JSON.parse(
    '{ "name": { "type": "string" }, "holder": { "__proto__": { "$ref": "#/name" } } }',
  );

  const { schema } = dereferenceSchema(input);

  assert.ok(isRecord(schema) && isRecord(schema.holder));
  assert.ok(Object.hasOwn(schema.holder, "__proto__"));
  assert.equal(Object.getPrototypeOf(schema.holder), Object.prototype);
  assert.deepEqual(
    Object.getOwnPropertyDescriptor(schema.holder, "__proto__")?.value,
    { type: "string" },
  );
});

test("leaves a recursive schema alone on request", () => {
  const { schema, circular } = dereferenceSchema(recursive, {
    circular: "ignore",
  });

  assert.deepEqual(schema, recursive);
  assert.deepEqual(circular, ["#/definitions/node"]);
});

test("merges sibling keywords deeply unless told otherwise", () => {
  const extended = {
    base: { properties: { id: { type: "integer" } } },
    named: { $ref: "#/base", properties: { name: { type: "string" } } },
  };

  assert.deepEqual(dereferenceSchema(extended).schema, {
    base: { properties: { id: { type: "integer" } } },
    named: {
      properties: { id: { type: "integer" }, name: { type: "string" } },
    },
  });
  assert.deepEqual(
    dereferenceSchema(extended, { siblings: "shallow" }).schema,
    {
      base: { properties: { id: { type: "integer" } } },
      named: { properties: { name: { type: "string" } } },
    },
  );
});

test("an options object can be used more than once", () => {
  const options = { maxDepth: 64 };

  assert.deepEqual(dereferenceSchema(recursive, options).circular, [
    "#/definitions/node",
  ]);
  assert.deepEqual(dereferenceSchema(recursive, options).circular, [
    "#/definitions/node",
  ]);
});

test("rejects an option it does not know", () => {
  const options = JSON.parse('{ "maxdepth": 64 }');

  throwsError(() => dereferenceSchema(recursive, options), "maxdepth");
});

test("says which reference failed and why", () => {
  throwsError(
    () => dereferenceSchema({ target: { $ref: "#/definitions/missing" } }),
    "cannot resolve reference `#/definitions/missing`: no member named `definitions`",
  );
  throwsError(
    () => dereferenceSchema({ target: { $ref: "other.json#/name" } }),
    "external reference `other.json#/name` is not supported",
  );
});

test("the text interface returns the same result as JSON", () => {
  const text = dereferenceSchemaText(JSON.stringify(recursive), {
    circular: "expand",
  });

  assert.deepEqual(
    JSON.parse(text),
    dereferenceSchema(recursive, { circular: "expand" }),
  );
  throwsError(() => dereferenceSchemaText("{"), "EOF");
});

test("rejects a document that nests too deeply to read", () => {
  let nested: unknown = null;
  for (let level = 0; level < 1_000; level += 1) {
    nested = [nested];
  }

  throwsError(() => dereferenceSchema(nested), "recursion limit exceeded");
});

test("rejects a document that is not JSON data", () => {
  const cyclic: { self?: unknown } = {};
  cyclic.self = cyclic;

  throwsError(() => dereferenceSchema(cyclic), "circular");
  throwsError(() => dereferenceSchema(undefined), "no JSON representation");
});

const order = "https://example.com/schemas/order.json";
const files: Record<string, unknown> = {
  [order]: {
    properties: {
      customer: { $ref: "customer.json#/definitions/name" },
      total: { $ref: "common/money.json" },
    },
  },
  "https://example.com/schemas/customer.json": {
    definitions: { name: { type: "string" } },
  },
  "https://example.com/schemas/common/money.json": {
    type: "number",
    currency: { $ref: "../customer.json#/definitions/name" },
  },
};

test("loads the documents a schema refers to", async () => {
  const asked: string[] = [];

  const { schema } = await dereferenceSchemaFrom(order, undefined, (url) => {
    asked.push(url);
    return files[url];
  });

  assert.deepEqual(schema, {
    properties: {
      customer: { type: "string" },
      total: { type: "number", currency: { type: "string" } },
    },
  });
  assert.deepEqual(asked.toSorted(), Object.keys(files).toSorted());
});

test("waits for a resolver that returns a promise", async () => {
  const { schema } = await dereferenceSchemaFrom(
    order,
    { circular: "reject" },
    (url) => Promise.resolve(files[url]),
  );

  assert.ok(isRecord(schema) && isRecord(schema.properties));
  assert.deepEqual(schema.properties.customer, { type: "string" });
});

test("rejects when a document cannot be loaded", async () => {
  await assert.rejects(
    dereferenceSchemaFrom(order, undefined, (url) => {
      if (url === order) {
        return files[url];
      }
      throw new Error(`no ${url}`);
    }),
    /no https:\/\/example\.com\/schemas\//,
  );
});

test("stops loading at the document limit", async () => {
  let loads = 0;

  await assert.rejects(
    dereferenceSchemaFrom(
      "https://example.com/chain/0.json",
      { maxDocuments: 5 },
      () => {
        loads += 1;
        return { next: { $ref: `${loads}.json` } };
      },
    ),
    /references lead through more than 5 documents/,
  );
  assert.equal(loads, 5);
});

/** Runs `action` with `fetch` answering from `answer`, and counts the calls. */
async function withFetch(
  answer: (url: string) => Response,
  action: () => Promise<void>,
): Promise<number> {
  const original = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = (input) => {
    calls += 1;
    return Promise.resolve(answer(String(input)));
  };
  try {
    await action();
  } finally {
    globalThis.fetch = original;
  }
  return calls;
}

test("fetches documents when given no resolver", async () => {
  const calls = await withFetch(
    (url) => new Response(JSON.stringify(files[url])),
    async () => {
      const { schema } = await dereferenceSchemaFrom(order);

      assert.ok(isRecord(schema) && isRecord(schema.properties));
      assert.deepEqual(schema.properties.customer, { type: "string" });
    },
  );

  assert.equal(calls, 3);
});

test("the default loader reads YAML", async () => {
  const yaml: Record<string, string> = {
    "https://example.com/api/root.yaml": [
      "title: Root",
      "responses:",
      "  200:",
      "    $ref: 'parts.yaml#/ok'",
      "",
    ].join("\n"),
    "https://example.com/api/parts.yaml": [
      "ok:",
      "  description: Fine",
      "  tags: [a, b]",
      "  nullable: false",
      "",
    ].join("\n"),
  };

  await withFetch(
    (url) => new Response(yaml[url]),
    async () => {
      const { schema } = await dereferenceSchemaFrom(
        "https://example.com/api/root.yaml",
      );

      assert.deepEqual(schema, {
        title: "Root",
        responses: {
          "200": { description: "Fine", tags: ["a", "b"], nullable: false },
        },
      });
    },
  );
});

test("the default loader refuses local and private addresses", async () => {
  const refused = [
    "http://localhost/schema.json",
    "http://localhost./schema.json",
    "http://127.0.0.1/schema.json",
    "http://2130706433/schema.json",
    "http://10.1.2.3/schema.json",
    "http://172.20.0.1/schema.json",
    "http://192.168.1.1/schema.json",
    "http://192.0.0.8/schema.json",
    "http://198.18.0.1/schema.json",
    "http://203.0.113.7/schema.json",
    "http://169.254.169.254/latest/meta-data",
    "http://[::1]/schema.json",
    "http://service.internal/schema.json",
    "http://metadata/schema.json",
    "file:///etc/passwd",
    "data:application/json,%7B%7D",
  ];

  const calls = await withFetch(
    () => new Response("{}"),
    async () => {
      for (const url of refused) {
        await assert.rejects(
          dereferenceSchemaFrom(url),
          /the default loader does not fetch/,
          url,
        );
      }
    },
  );

  assert.equal(calls, 0);
});

test("the default loader checks where a redirect leads", async () => {
  const calls = await withFetch(
    () =>
      new Response(null, {
        status: 302,
        headers: { location: "http://169.254.169.254/latest/meta-data" },
      }),
    async () => {
      await assert.rejects(
        dereferenceSchemaFrom(order),
        /does not fetch http:\/\/169\.254\.169\.254\/latest\/meta-data, which names a local or private host/,
      );
    },
  );

  assert.equal(calls, 1);
});

test("the default loader stays on the origin it was given", async () => {
  const calls = await withFetch(
    () =>
      new Response(
        JSON.stringify({ other: { $ref: "https://elsewhere.example/x.json" } }),
      ),
    async () => {
      await assert.rejects(
        dereferenceSchemaFrom(order),
        /does not fetch https:\/\/elsewhere\.example\/x\.json, which is on another origin/,
      );
    },
  );

  assert.equal(calls, 1);
});

const fixture = (name: string): string =>
  new URL(`./fixtures/${name}`, import.meta.url).href;

const fromFolder: { load: LoadOptions } = { load: { files: "folder" } };

test("reads a document and what it refers to from disk", async () => {
  const path = fileURLToPath(fixture("order.json"));

  const calls = await withFetch(
    () => new Response("{}"),
    async () => {
      for (const location of [
        fixture("order.json"),
        path,
        relative(process.cwd(), path),
      ]) {
        const { schema } = await dereferenceSchemaFrom(location, fromFolder);

        assert.deepEqual(
          schema,
          {
            title: "Order",
            properties: {
              customer: {
                type: "object",
                properties: { id: { type: "integer" } },
              },
            },
          },
          location,
        );
      }
    },
  );

  assert.equal(calls, 0);
});

test("says which file is not there", async () => {
  await assert.rejects(
    dereferenceSchemaFrom(fixture("absent.json"), fromFolder),
    /ENOENT/,
  );
});

test("the default loader takes a path for a URL and refuses it", async () => {
  const calls = await withFetch(
    () => new Response("{}"),
    async () => {
      for (const path of ["/etc/passwd", "schemas/order.json"]) {
        await assert.rejects(
          dereferenceSchemaFrom(path),
          /is not a usable URL/,
          path,
        );
      }
    },
  );

  assert.equal(calls, 0);
});

test("a file leads only to files", async () => {
  const calls = await withFetch(
    () => new Response("{}"),
    async () => {
      await assert.rejects(
        dereferenceSchemaFrom(fixture("remote.json"), fromFolder),
        /does not fetch https:\/\/example\.com\/schemas\/order\.json, which is on another origin than file:/,
      );
    },
  );

  assert.equal(calls, 0);
});

test("a file leads only to files on this machine", async () => {
  await assert.rejects(
    dereferenceSchemaFrom(fixture("share.json"), fromFolder),
    /does not read file:\/\/files\.example\/share\/schema\.json, which names another machine/,
  );
});

test("a file leads only to files in its own folder", async () => {
  await assert.rejects(
    dereferenceSchemaFrom(fixture("parts/escape.json"), fromFolder),
    /does not read file:.*\/fixtures\/shared\.json, which is outside /,
  );
});

test("reads from the folder it is given", async () => {
  const { schema } = await dereferenceSchemaFrom(fixture("parts/escape.json"), {
    load: { files: "folder", folder: fileURLToPath(fixture("")) },
  });

  assert.deepEqual(schema, { above: { type: "integer" } });

  await assert.rejects(
    dereferenceSchemaFrom(fixture("order.json"), {
      load: { files: "folder", folder: fixture("parts/") },
    }),
    /order\.json, which is outside /,
  );
});

test("a file leads only to files named as documents", async () => {
  await assert.rejects(
    dereferenceSchemaFrom(fixture("notes.json"), fromFolder),
    /notes\.txt, which is not a \.json, \.yaml, \.yml file/,
  );
});

test("a link does not lead out of the folder", async () => {
  const folder = await mkdtemp(join(tmpdir(), "unbinder-"));
  try {
    await symlink(
      fileURLToPath(fixture("shared.json")),
      join(folder, "linked.json"),
    );
    await writeFile(
      join(folder, "root.json"),
      JSON.stringify({ id: { $ref: "linked.json#/definitions/id" } }),
    );

    await assert.rejects(
      dereferenceSchemaFrom(join(folder, "root.json"), fromFolder),
      /linked\.json, which is a link that leads outside /,
    );

    await symlink(
      fileURLToPath(fixture("notes.txt")),
      join(folder, "renamed.json"),
    );
    await writeFile(
      join(folder, "renamed-root.json"),
      JSON.stringify({ notes: { $ref: "renamed.json" } }),
    );

    await assert.rejects(
      dereferenceSchemaFrom(join(folder, "renamed-root.json"), {
        load: {
          files: "folder",
          folder: fileURLToPath(new URL("/", fixture(""))),
        },
      }),
      /renamed\.json, which is not a \.json, \.yaml, \.yml file/,
    );
  } finally {
    await rm(folder, { recursive: true });
  }
});

test("a fetched document cannot make the default loader read a file", async () => {
  const calls = await withFetch(
    () =>
      new Response(JSON.stringify({ secrets: { $ref: fixture("shared.json") } })),
    async () => {
      await assert.rejects(
        dereferenceSchemaFrom(order),
        /the default loader does not fetch file:.*shared\.json, which is not an http or https URL/,
      );
    },
  );

  assert.equal(calls, 1);
});

test("stops reading a file that is too large", async () => {
  await assert.rejects(
    dereferenceSchemaFrom(fixture("order.json"), {
      maxBytes: 16,
      load: { files: "folder" },
    }),
    /order\.json is larger than the 16 bytes left to load/,
  );
});

test("reads any file a document names when told to", async () => {
  const anyFile: { load: LoadOptions } = { load: { files: "any" } };

  const above = await dereferenceSchemaFrom(
    fixture("parts/escape.json"),
    anyFile,
  );

  assert.deepEqual(above.schema, { above: { type: "integer" } });
});

test("takes a document named as text for the string it holds on request", async () => {
  const read = await dereferenceSchemaFrom(fixture("notes.json"), {
    load: { files: "any" },
  });
  const verbatim = await dereferenceSchemaFrom(fixture("notes.json"), {
    load: { files: "any", text: true },
  });

  assert.deepEqual(read.schema, { notes: "These are notes, not a schema." });
  assert.deepEqual(verbatim.schema, {
    notes: "These are notes, not a schema.\n",
  });
});

test("reads a fetched document named as text as a document", async () => {
  const fetched: Record<string, string> = {
    [order]: JSON.stringify({ part: { $ref: "part.txt" } }),
    "https://example.com/schemas/part.txt": JSON.stringify({ type: "null" }),
  };

  await withFetch(
    (url) => new Response(fetched[url]),
    async () => {
      const { schema } = await dereferenceSchemaFrom(order);

      assert.deepEqual(schema, { part: { type: "null" } });
    },
  );
});

test("a fetched document may name a file when any file may be read", async () => {
  await withFetch(
    () => new Response(JSON.stringify({ id: { $ref: fixture("shared.json") } })),
    async () => {
      const { schema } = await dereferenceSchemaFrom(order, {
        load: { files: "any" },
      });

      assert.deepEqual(schema, {
        id: { definitions: { id: { type: "integer" } } },
      });
    },
  );
});

const elsewhere: Record<string, unknown> = {
  [order]: { part: { $ref: "https://elsewhere.example/part.json" } },
  "https://elsewhere.example/part.json": { type: "boolean" },
};

test("fetches from other origins when told to", async () => {
  const calls = await withFetch(
    (url) => new Response(JSON.stringify(elsewhere[url])),
    async () => {
      const { schema } = await dereferenceSchemaFrom(order, {
        load: { sameOrigin: false, safeUrlResolver: false },
      });

      assert.deepEqual(schema, { part: { type: "boolean" } });
    },
  );

  assert.equal(calls, 2);
});

test("fetches from a private host when told to", async () => {
  const calls = await withFetch(
    () => new Response(JSON.stringify({ type: "null" })),
    async () => {
      const { schema } = await dereferenceSchemaFrom(
        "http://127.0.0.1:8080/schema.json",
        { load: { safeUrlResolver: false } },
      );

      assert.deepEqual(schema, { type: "null" });
    },
  );

  assert.equal(calls, 1);
});

test("refuses a public name that leads to a private address", async (context) => {
  const answers: Checked[] = [
    { address: "10.0.0.5", family: 4 },
    { address: "192.0.2.1", family: 4 },
    { address: "::1", family: 6 },
    { address: "fe80::1", family: 6 },
    { address: "::ffff:10.0.0.5", family: 6 },
    { address: "64:ff9b::a00:5", family: 6 },
    { address: "2002:a00:5::1", family: 6 },
  ];
  let answer = 0;
  context.mock.method(dns, "lookup", async () => [
    { address: "93.184.215.14", family: 4 },
    answers[answer],
  ]);

  const calls = await withFetch(
    () => new Response("{}"),
    async () => {
      for (answer = 0; answer < answers.length; answer += 1) {
        await assert.rejects(
          dereferenceSchemaFrom(order, { load: { sameOrigin: false } }),
          /does not fetch https:\/\/example\.com\/schemas\/order\.json, which leads to a local or private address/,
          answers[answer]?.address,
        );
      }
    },
  );

  assert.equal(calls, 0);
});

test("refuses another origin where a host cannot be looked up", async (context) => {
  context.mock.method(process, "getBuiltinModule", () => undefined);
  // The globals of a page do not make a server one.
  for (const name of ["window", "document"]) {
    Object.defineProperty(globalThis, name, { value: {}, configurable: true });
  }

  try {
    const calls = await withFetch(
      () => new Response("{}"),
      async () => {
        await assert.rejects(
          dereferenceSchemaFrom(order, { load: { sameOrigin: false } }),
          /which leads somewhere this runtime cannot check/,
        );
      },
    );

    assert.equal(calls, 0);
  } finally {
    for (const name of ["window", "document"]) {
      Reflect.deleteProperty(globalThis, name);
    }
  }
});

interface Checked {
  address: string;
  family: number;
}

interface Pinned {
  lookup: (
    hostname: string,
    options: { all: boolean },
    callback: (error: Error | null, addresses: Checked[]) => void,
  ) => void;
}

test("connects to the address that was checked", async (context) => {
  const answers: Record<string, string> = {
    "example.com": "93.184.215.14",
    "elsewhere.example": "93.184.215.15",
  };
  const connected: string[] = [];
  context.mock.method(dns, "lookup", async (hostname: string) => [
    { address: answers[hostname], family: 4 },
  ]);
  context.mock.method(
    https,
    "request",
    (url: string, options: Pinned, answer: (response: Readable) => void) => ({
      on() {},
      end() {
        options.lookup(new URL(url).hostname, { all: true }, (_, addresses) => {
          connected.push(...addresses.map(({ address }) => address));
        });
        answer(
          Object.assign(
            Readable.from([Buffer.from(JSON.stringify(elsewhere[url]))]),
            {
              statusCode: 200,
              headers: {},
            },
          ),
        );
      },
    }),
  );

  const { schema } = await dereferenceSchemaFrom(order, {
    load: { sameOrigin: false },
  });

  assert.deepEqual(schema, { part: { type: "boolean" } });
  assert.deepEqual(connected, ["93.184.215.14", "93.184.215.15"]);
});

test("rewrites the schema it was given on request", () => {
  const untouched = structuredClone(recursive);
  dereferenceSchema(untouched);
  assert.deepEqual(untouched, recursive);

  const input: Record<string, unknown> = {
    ...structuredClone(recursive),
    whole: { $ref: "#" },
  };
  const { schema } = dereferenceSchema(input, { mutateInputSchema: true });

  assert.equal(schema, input);
  assert.equal(input.whole, input);
  assert.ok(isRecord(input.list) && isRecord(input.list.properties));
  assert.equal(input.list.properties.next, input.list);
});

test("drops the keywords next to a reference to true on request", () => {
  const input = {
    anything: true,
    bounded: { $ref: "#/anything", maxLength: 5 },
  };

  assert.deepEqual(dereferenceSchema(input).schema, {
    anything: true,
    bounded: { maxLength: 5 },
  });
  assert.deepEqual(dereferenceSchema(input, { trueTarget: "replace" }).schema, {
    anything: true,
    bounded: true,
  });
});

test("a pointer may leave the slashes of a name unescaped", () => {
  const { schema } = dereferenceSchema({
    paths: { "/users/{id}": { get: { summary: "One user" } } },
    link: { $ref: "#/paths//users/{id}/get" },
  });

  assert.ok(isRecord(schema));
  assert.deepEqual(schema.link, { summary: "One user" });
});

test("the default loader stops reading a document that is too large", async () => {
  await withFetch(
    () => new Response(JSON.stringify({ padding: "x".repeat(10_000) })),
    async () => {
      await assert.rejects(
        dereferenceSchemaFrom(order, { maxBytes: 1_000 }),
        /is larger than the 1000 bytes left to load/,
      );
    },
  );
});

test("counts the documents a resolver returns against the size limit", async () => {
  await assert.rejects(
    dereferenceSchemaFrom(
      order,
      { maxBytes: 1_000 },
      () => ({ padding: "x".repeat(10_000) }),
    ),
    /output takes more than 1000 bytes/,
  );
});

test("counts containers and references", () => {
  assert.deepEqual(schemaStats(recursive), {
    objects: 6,
    arrays: 0,
    references: 2,
  });
});
