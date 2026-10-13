import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

import init, {
  dereferenceSchema,
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

test("expands a recursive schema one level and reports the cycle", () => {
  const { schema, circular } = dereferenceSchema(recursive);

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

  assert.deepEqual(JSON.parse(text), dereferenceSchema(recursive));
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

test("counts containers and references", () => {
  assert.deepEqual(schemaStats(recursive), {
    objects: 6,
    arrays: 0,
    references: 2,
  });
});
