import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

import init, {
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

test("the default loader refuses local and private addresses", async () => {
  const refused = [
    "http://localhost/schema.json",
    "http://localhost./schema.json",
    "http://127.0.0.1/schema.json",
    "http://2130706433/schema.json",
    "http://10.1.2.3/schema.json",
    "http://172.20.0.1/schema.json",
    "http://192.168.1.1/schema.json",
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
