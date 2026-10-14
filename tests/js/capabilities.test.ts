import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import $RefParser from "@apidevtools/json-schema-ref-parser";

import init, {
  type LoadOptions,
  bundleSchema,
  bundleSchemaFrom,
  dereferenceSchema,
  dereferenceSchemaFrom,
  resolveSchemaFrom,
} from "../../pkg/unbinder.js";

await init({
  module_or_path: await readFile(
    new URL("../../pkg/unbinder_bg.wasm", import.meta.url),
  ),
});

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const bundleFixture = (name: string): string =>
  fileURLToPath(new URL(`./fixtures/bundle/${name}`, import.meta.url));

const anyFile: { load: LoadOptions } = { load: { files: "any" } };

test("bundles the way json-schema-ref-parser bundles", async () => {
  const root = bundleFixture("root.json");

  const { schema } = await bundleSchemaFrom(root, anyFile);

  assert.deepEqual(schema, await $RefParser.bundle(root));
});

test("bundles references within the document as written on request", async () => {
  const root = bundleFixture("root.json");

  const { schema } = await bundleSchemaFrom(root, { ...anyFile, internal: "written" });

  assert.deepEqual(
    schema,
    await $RefParser.bundle(root, { bundle: { optimizeInternalRefs: false } }),
  );
});

test("dereferences documents that refer to one another as json-schema-ref-parser does", async () => {
  const root = bundleFixture("root.json");

  const { schema } = await dereferenceSchemaFrom(root, anyFile);

  assert.deepEqual(schema, await $RefParser.dereference(root));
});

test("leaves cycles alone as json-schema-ref-parser does on request", async () => {
  const root = bundleFixture("root.json");

  const { schema } = await dereferenceSchemaFrom(root, { ...anyFile, circular: "ignore" });

  assert.deepEqual(
    schema,
    await $RefParser.dereference(root, { dereference: { circular: "ignore" } }),
  );
});

test("leaves what a matcher excludes as json-schema-ref-parser does", async () => {
  const root = bundleFixture("root.json");
  const excluded = (path: string) => path.includes("/examples");

  const { schema } = await dereferenceSchemaFrom(root, { ...anyFile, excluded });
  const bundled = await bundleSchemaFrom(root, { ...anyFile, excluded });

  assert.deepEqual(
    schema,
    await $RefParser.dereference(root, { dereference: { excludedPathMatcher: excluded } }),
  );
  assert.deepEqual(
    bundled.schema,
    await $RefParser.bundle(root, { bundle: { excludedPathMatcher: excluded } }),
  );
});

test("leaves references to other documents as written on request", async () => {
  const root = bundleFixture("root.json");
  const asked: string[] = [];

  const { schema } = await dereferenceSchemaFrom(
    "https://example.com/root.json",
    { external: "ignore" },
    (url) => {
      asked.push(url);
      return { remote: { $ref: "other.json" }, local: { $ref: "#/d" }, d: 1 };
    },
  );

  assert.deepEqual(schema, { remote: { $ref: "other.json" }, local: 1, d: 1 });
  assert.deepEqual(asked, ["https://example.com/root.json"]);
  assert.deepEqual(
    (await dereferenceSchemaFrom(root, { ...anyFile, external: "ignore" })).schema,
    await $RefParser.dereference(root, { resolve: { external: false } }),
  );
});

test("resolves references to other documents against a base on request", async () => {
  const asked: string[] = [];

  const { schema } = await dereferenceSchemaFrom(
    "https://example.com/schemas/root.json",
    { externalBase: "https://example.com/base/" },
    (url) => {
      asked.push(url);
      return url.endsWith("root.json") ? { name: { $ref: "name.json" } } : { type: "string" };
    },
  );

  assert.deepEqual(schema, { name: { type: "string" } });
  assert.deepEqual(asked, [
    "https://example.com/schemas/root.json",
    "https://example.com/base/name.json",
  ]);
});

test("follows only JSON Pointers within a document on request", () => {
  const input = { named: { $ref: "#name" }, empty: { $ref: "" }, d: { $ref: "#/e" }, e: 1 };

  const { schema } = dereferenceSchema(input, { local: "pointers" });

  assert.deepEqual(schema, { named: { $ref: "#name" }, empty: { $ref: "" }, d: 1, e: 1 });
});

test("asks the matcher with each place and the value written there", () => {
  const input = {
    name: { $ref: "#/definitions/name" },
    examples: [{ $ref: "#/definitions/name" }],
    definitions: { name: { type: "string" } },
  };
  const seen = new Map<string, unknown>();

  const { schema } = dereferenceSchema(input, {
    excluded: (path, value) => {
      seen.set(path, value);
      return path === "#/examples";
    },
  });

  assert.ok(isRecord(schema));
  assert.deepEqual(schema.examples, input.examples);
  assert.equal(seen.get("#/examples"), input.examples);
  assert.equal(seen.get("#/definitions"), input.definitions);
  assert.deepEqual(schema.name, { type: "string" });
});

test("tells the callbacks what it replaced and which references are circular", () => {
  const replaced: [string, unknown, unknown, string | undefined][] = [];
  const circular: string[] = [];

  const { schema } = dereferenceSchema(
    {
      definitions: {
        node: { properties: { next: { $ref: "#/definitions/node" } } },
      },
      list: { $ref: "#/definitions/node" },
    },
    {
      onDereference: (reference, value, parent, key) => replaced.push([reference, value, parent, key]),
      onCircular: (source) => circular.push(source),
    },
  );

  assert.ok(isRecord(schema) && isRecord(schema.definitions));
  for (const [reference, value, parent, key] of replaced) {
    assert.equal(reference, "#/definitions/node");
    assert.ok(isRecord(parent) && key !== undefined);
    assert.equal(parent[key], value);
    assert.equal(value, schema.definitions.node);
  }
  assert.ok(replaced.some(([, , parent]) => parent === schema));
  assert.deepEqual(circular, ["#/definitions/node/properties/next"]);
});

test("carries on past references that do not resolve and lists them", async () => {
  const root = bundleFixture("missing.json");

  const result = await dereferenceSchemaFrom(root, { ...anyFile, continueOnError: true });

  assert.deepEqual(result.schema, { found: { type: "string" }, lost: null, gone: null });
  assert.deepEqual(
    result.errors?.map(({ error, source }) => [error.kind, source.replace(/^.*#/, "#")]),
    [
      ["unresolvedReference", "#/lost"],
      ["unresolvedReference", "#/gone"],
    ],
  );
  await assert.rejects(dereferenceSchemaFrom(root, anyFile), /cannot resolve reference/);
});

test("stops a run that takes longer than it is given", () => {
  const busy = (): boolean => {
    const until = Date.now() + 5;
    while (Date.now() < until) {
      // Waits out the time limit.
    }
    return false;
  };

  assert.throws(
    () => dereferenceSchema({ a: [1, 2, 3], b: { c: 1 } }, { timeoutMs: 1, excluded: busy }),
    /took longer than 1 ms/,
  );
});

test("a callback that throws stops the run with what it threw", () => {
  const thrown = new Error("no");

  assert.throws(
    () =>
      dereferenceSchema({ a: 1 }, {
        excluded: () => {
          throw thrown;
        },
      }),
    (error: unknown) => error === thrown,
  );
});

test("gives each reference its own copy on request, and keeps cycles linked", () => {
  const { schema } = dereferenceSchema(
    {
      first: { $ref: "#/definitions/name" },
      second: { $ref: "#/definitions/name" },
      list: { $ref: "#/definitions/node" },
      definitions: {
        name: { type: "string" },
        node: { properties: { next: { $ref: "#/definitions/node" } } },
      },
    },
    { cloneReferences: true },
  );

  assert.ok(isRecord(schema) && isRecord(schema.definitions) && isRecord(schema.list));
  assert.deepEqual(schema.first, schema.second);
  assert.notEqual(schema.first, schema.second);
  assert.notEqual(schema.first, schema.definitions.name);
  assert.ok(isRecord(schema.list.properties));
  assert.equal(schema.list.properties.next, schema.definitions.node);
});

test("bundles a schema on its own and into itself on request", () => {
  const input = {
    name: { $ref: "#/alias" },
    alias: { $ref: "#/definitions/name" },
    definitions: { name: { type: "string" } },
  };
  const looked: [unknown, unknown, unknown, string | undefined][] = [];

  const { schema } = bundleSchema(input, {
    mutateInputSchema: true,
    onBundle: (reference, value, parent, key) => looked.push([reference, value, parent, key]),
  });

  assert.equal(schema, input);
  assert.deepEqual(input.name, { $ref: "#/definitions/name" });
  assert.deepEqual(
    looked.map(([reference, , , key]) => [reference, key]),
    [
      ["#/alias", "name"],
      ["#/definitions/name", "alias"],
    ],
  );
});

test("loads documents without dereferencing them, and reads and writes them", async () => {
  const root = bundleFixture("root.json");

  const resolved = await resolveSchemaFrom(root, anyFile);

  assert.deepEqual(
    resolved.urls().map((url) => url.replace(/^.*\//, "")),
    ["people.yaml", "pet.json", "root.json"],
  );
  assert.equal(resolved.get("#/title"), "Root");
  assert.deepEqual(resolved.get("#/properties/petName"), { type: "string" });
  assert.deepEqual(resolved.get("people.yaml#/Person/properties/name"), { type: "string" });
  assert.ok(resolved.exists("#/properties/owner/properties/pets"));
  assert.ok(!resolved.exists("#/nowhere"));

  resolved.set("#/title", "Renamed");
  resolved.set("pet.json#/properties/age", { type: "integer" });

  const rootDocument = resolved.toJSON()[resolved.url];
  assert.ok(isRecord(rootDocument));
  assert.equal(rootDocument.title, "Renamed");
  const { schema } = resolved.dereference();
  assert.ok(isRecord(schema) && isRecord(schema.properties) && isRecord(schema.properties.pet));
  assert.deepEqual(schema.properties.pet.properties, {
    name: { type: "string" },
    owner: schema.properties.owner,
    age: { type: "integer" },
  });
  assert.ok(isRecord(resolved.bundle().schema));
});

test("loads only the document asked for when told to leave other documents", async () => {
  const resolved = await resolveSchemaFrom(bundleFixture("root.json"), {
    ...anyFile,
    external: "ignore",
  });

  assert.equal(resolved.urls().length, 1);
});

test("sends the headers it is given and follows only as many redirects as allowed", async (context) => {
  const sent: Headers[] = [];
  context.mock.method(globalThis, "fetch", (_: unknown, init?: RequestInit) => {
    sent.push(new Headers(init?.headers));
    return Promise.resolve(
      new Response(null, { status: 302, headers: { location: "https://example.com/next.json" } }),
    );
  });

  await assert.rejects(
    dereferenceSchemaFrom("https://example.com/root.json", {
      load: { headers: { authorization: "Bearer token" }, redirects: 1 },
    }),
    /redirects more than 1 times/,
  );
  assert.equal(sent.length, 2);
  assert.equal(sent[0]?.get("authorization"), "Bearer token");
});

test("drops the headers it is given on a redirect to another origin", async (context) => {
  const sent: [string, Headers][] = [];
  context.mock.method(globalThis, "fetch", (url: unknown, init?: RequestInit) => {
    sent.push([String(url), new Headers(init?.headers)]);
    return Promise.resolve(
      sent.length === 1
        ? new Response(null, { status: 302, headers: { location: "https://other.example/x.json" } })
        : new Response("{}"),
    );
  });

  await dereferenceSchemaFrom("https://example.com/root.json", {
    load: {
      sameOrigin: false,
      safeUrlResolver: false,
      headers: { authorization: "Bearer token", accept: "application/json" },
    },
  });

  assert.equal(sent[1]?.[0], "https://other.example/x.json");
  assert.equal(sent[1]?.[1].get("authorization"), null);
  assert.equal(sent[1]?.[1].get("accept"), null);
});

test("sends its headers and credentials only to the origin it was asked for", async (context) => {
  const sent: [string, Headers][] = [];
  const credentials = new Map<string, RequestCredentials | undefined>();
  context.mock.method(globalThis, "fetch", (url: unknown, init?: RequestInit) => {
    sent.push([String(url), new Headers(init?.headers)]);
    credentials.set(String(url), init?.credentials);
    return Promise.resolve(
      new Response(
        String(url).includes("other.example")
          ? "{}"
          : JSON.stringify({ elsewhere: { $ref: "https://other.example/x.json" } }),
      ),
    );
  });

  await dereferenceSchemaFrom("https://example.com/root.json", {
    load: {
      sameOrigin: false,
      safeUrlResolver: false,
      headers: { "x-api-key": "secret", cookie: "session=1", accept: "application/json" },
      credentials: "include",
    },
  });

  const headers = new Map(sent);
  assert.equal(headers.get("https://example.com/root.json")?.get("x-api-key"), "secret");
  assert.equal(credentials.get("https://example.com/root.json"), "include");
  assert.equal(headers.get("https://other.example/x.json")?.get("x-api-key"), null);
  assert.equal(headers.get("https://other.example/x.json")?.get("cookie"), null);
  assert.equal(headers.get("https://other.example/x.json")?.get("accept"), null);
  assert.equal(credentials.get("https://other.example/x.json"), "same-origin");
});
