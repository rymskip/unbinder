# Unbinder

A high-performance JSON Schema dereferencer written in Rust with WebAssembly
support. Unbinder resolves `$ref` pointers in JSON Schema documents, making them
self-contained by replacing references with their actual content.

## Features

-   **Complete**: inlines every reference, including those inside targets and
    references to references
-   **Fast**: ~4.3x faster than json-schema-ref-parser
-   **Same on both sides**: a Rust server and a WebAssembly frontend validate
    against the same dereferenced schema
-   **Matches json-schema-ref-parser**: everything it does is an option here,
    bundling included
-   **Finite on recursive schemas**: cycles are linked or expanded one level,
    and reported
-   **Standards-based pointers**: RFC 6901, with `~0`/`~1` escapes,
    percent-encoding, and names whose `/` was left unescaped
-   **Bounded**: depth, size and document limits stop hostile input; analysis
    is linear
-   **Typed errors**: each failure names the reference and the reason
-   **Portable**: `no_std` with `alloc`, no `unsafe`, two direct dependencies
-   **WebAssembly bindings**: on npm and JSR, with generated TypeScript types

## Installation

```toml
[dependencies]
unbinder = "0.2"
# without the standard library:
# unbinder = { version = "0.2", default-features = false }
```

```bash
npm install unbinder
npx jsr add @rymskip/unbinder
```

## Usage

### Rust

```rust
use serde_json::json;
use unbinder::dereference_schema;

fn main() -> Result<(), unbinder::Error> {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "$ref": "#/definitions/name" },
            "age": { "$ref": "#/definitions/age", "description": "Age in years" }
        },
        "definitions": {
            "name": { "type": "string" },
            "age": { "type": "integer", "minimum": 0 }
        }
    });

    let dereferenced = dereference_schema(&schema)?;

    assert_eq!(
        dereferenced.schema["properties"],
        json!({
            "name": { "type": "string" },
            "age": { "type": "integer", "minimum": 0, "description": "Age in years" }
        })
    );
    Ok(())
}
```

The input is borrowed, not changed. Keywords next to a `$ref` are merged over
the target, so a sibling `description` wins. A pointer passes through any
`$ref` on its way. Every choice json-schema-ref-parser makes is a default in
`Options`, passed to `dereference_schema_with`.

### Without copying

`dereference_schema` gives each reference its own copy of its target, which
can make the output far larger than the input. `dereference_schema_shared`
holds each target once, borrows the rest, and serializes straight to the full
output with any serde format.

```rust
use serde_json::json;
use unbinder::{Options, dereference_schema_shared};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = json!({
        "address": { "type": "object" },
        "billing": { "$ref": "#/address" },
        "shipping": { "$ref": "#/address" }
    });

    let shared = dereference_schema_shared(&schema, &Options::default())?;

    assert_eq!(
        serde_json::to_string(&shared)?,
        r#"{"address":{"type":"object"},"billing":{"type":"object"},"shipping":{"type":"object"}}"#
    );
    Ok(())
}
```

### References to other documents

`Documents` holds documents by URL and lists the ones still to load with
`missing()`. Loading is up to you: the crate opens no files or connections.

```rust
use std::collections::BTreeMap;

use serde_json::{Value, json};
use unbinder::{Documents, Options};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Stands in for a disk or a network.
    let files = BTreeMap::from([
        (
            "https://example.com/schemas/order.json",
            json!({ "customer": { "$ref": "customer.json#/definitions/name" } }),
        ),
        (
            "https://example.com/schemas/customer.json",
            json!({ "definitions": { "name": { "type": "string" } } }),
        ),
    ]);
    let load = |url: &str| -> Result<Value, String> {
        files.get(url).cloned().ok_or(format!("no document at {url}"))
    };

    let root = "https://example.com/schemas/order.json";
    let mut documents = Documents::new();
    documents.insert(root, load(root)?)?;
    while let Some(url) = documents.missing().first() {
        documents.insert(url, load(url)?)?;
    }

    let order = documents.dereference(root, &Options::default())?;
    assert_eq!(order.to_value(), json!({ "customer": { "type": "string" } }));
    Ok(())
}
```

In JavaScript, `dereferenceSchemaFrom` loads documents as JSON or YAML, or
through a `resolve` function you pass:

```javascript
import init, { dereferenceSchemaFrom } from "unbinder";

await init();

const { schema } = await dereferenceSchemaFrom("https://example.com/schemas/order.json");

// From disk, in Node.js, Bun and Deno.
await dereferenceSchemaFrom("schemas/order.json", { load: { files: "folder" } });

// What json-schema-ref-parser loads: any public origin, any file, text as strings.
await dereferenceSchemaFrom("schemas/order.json", {
    load: { sameOrigin: false, files: "any", text: true },
});

// Your own loader.
await dereferenceSchemaFrom("https://example.com/schemas/order.json", {}, async (url) =>
    (await fetch(url, { headers: { accept: "application/schema+json" } })).json(),
);
```

By default the loader fetches only `http(s)` URLs on the origin you gave,
never a private or local host, reads no files, holds redirects to the same
rules, and stops at `maxBytes`. `load` changes that:

| `load`            | Default        | Meaning                                                  |
| ----------------- | -------------- | -------------------------------------------------------- |
| `sameOrigin`      | `true`         | Fetch only from the origin you gave; off, hosts are looked up and pinned to checked public addresses |
| `safeUrlResolver` | `true`         | Refuse local and private hosts                           |
| `files`           | `"none"`       | `"folder"`: `.json`/`.yaml`/`.yml` in one folder, links can't escape; `"any"`: any file |
| `folder`          | file's folder  | The folder for `"folder"`                                |
| `text`            | `false`        | Read `.txt`, `.md` and the like as strings               |
| `headers`         |                | Sent with each request                                   |
| `timeoutMs`       | 60000          | Per request                                              |
| `redirects`       | 5              | Per document                                             |
| `credentials`     | `"same-origin"`| As in `fetch`                                            |

`headers` and `credentials` go only to the origin you gave, so a document
can't send them elsewhere; a `resolve` function decides for itself.

An `$id` gives a document, or part of one, its own URL, and pointers inside it
start there. As in json-schema-ref-parser, ids count only in a document whose
root has one or names a JSON Schema draft; draft 4's `id` works too.

### Circular references

A tree or text can't hold a cycle, so there a reference in one is inlined
from outside the cycle and left as `$ref` inside it, and listed in
`circular`. JavaScript objects can, so `dereferenceSchema` links the cycle as
json-schema-ref-parser does (`schema.list.properties.next === schema.list`);
`circular: "expand"` expands it instead.

```rust
use serde_json::json;
use unbinder::{CircularPolicy, Error, Options, dereference_schema, dereference_schema_with};

fn main() -> Result<(), Error> {
    let schema = json!({
        "definitions": {
            "node": { "properties": { "next": { "$ref": "#/definitions/node" } } }
        },
        "list": { "$ref": "#/definitions/node" }
    });

    let dereferenced = dereference_schema(&schema)?;

    assert_eq!(
        dereferenced.schema["list"],
        json!({ "properties": { "next": { "$ref": "#/definitions/node" } } })
    );
    assert_eq!(dereferenced.circular, ["#/definitions/node"]);

    // Leave every reference that leads to a cycle exactly as it is.
    let untouched = Options {
        circular: CircularPolicy::Ignore,
        ..Options::default()
    };
    assert_eq!(dereference_schema_with(&schema, &untouched)?.schema, schema);

    // Or treat a cycle as an error.
    let strict = Options {
        circular: CircularPolicy::Reject,
        ..Options::default()
    };
    assert_eq!(
        dereference_schema_with(&schema, &strict),
        Err(Error::CircularReference {
            reference: "#/definitions/node".to_owned()
        })
    );
    Ok(())
}
```

### Options

| Field           | Default  | Meaning                                                     |
| --------------- | -------- | ----------------------------------------------------------- |
| `circular`      | `Link`   | `Link` (JavaScript objects; `Expand` elsewhere), `Expand` one level, `Ignore`, `Reject` |
| `siblings`      | `Deep`   | Merge sibling objects into the target's inward, or replace them (`Shallow`); other values always replace |
| `true_target`   | `Keep`   | Keep the keywords next to a `$ref` to `true`, or `Replace` them with `true` |
| `max_depth`     | 128      | Output nesting; dereferencing recurses per level, so mind the stack |
| `max_bytes`     | 256 MiB  | Output size as a tree, checked on the shared form before writing |
| `max_documents` | 1024     | Documents the references may lead through                   |
| `external`      | `Follow` | `Ignore` leaves references to other documents as written    |
| `local`         | `Any`    | `Pointers` leaves `#name` and an empty `$ref` as written    |
| `external_base` | none     | Resolve references to other documents against this URL      |
| `internal`      | `Direct` | Bundling: point internal references at their final target, or leave them `Written` |

Exceeding a limit returns the matching `Error::*LimitExceeded`.

### Hooks

`dereference_schema_with_hooks`, `Documents::dereference_with_hooks` and the
`bundle*_with_hooks` functions take a `Hooks` implementation. Every method
defaults to doing nothing:

| Method         | Called                                                         |
| -------------- | -------------------------------------------------------------- |
| `excluded`     | For each value; `true` leaves it and its contents as written   |
| `circular`     | For each reference in a cycle                                  |
| `dereferenced` | For each reference replaced                                    |
| `bundled`      | For each member with a `$ref` a bundle looks through           |
| `failed`       | For an unresolvable reference; `Ok` puts `null` there and carries on |
| `proceed`      | Before each value; an error stops the run, e.g. on a time limit |

Each gets a `Place`: its output path, the value as written, and `source()`.

### Bundling

`bundle_schema` and `Documents::bundle` produce one document whose references
all point inside it, without inlining them. Each value from another document
is written once, where json-schema-ref-parser would put it, and the other
references to it point there. Internal references point straight at their
targets. `Documents::lookup` reads the value a URL with a pointer leads to,
and `Documents::set` writes one.

### JavaScript/TypeScript

Initialize the module first:

```javascript
import init, { dereferenceSchema } from "unbinder";

await init();

const { schema, circular } = dereferenceSchema({
    properties: { name: { $ref: "#/definitions/name" } },
    definitions: { name: { type: "string" } },
});
```

Places that refer to one target share one object, as in
json-schema-ref-parser, so the result is about the document's size. The input
is left alone unless `mutateInputSchema` is set. `dereferenceSchemaText`
returns JSON text with a copy per reference. Options are a plain object;
failures throw `Error` with the kind in `error.detail`.

Everything that dereferences or bundles also takes:

| Option            | Meaning                                                      |
| ----------------- | ------------------------------------------------------------ |
| `excluded(path, value)` | `true` leaves that value and its contents as written   |
| `onDereference(reference, value, parent, key)` | Each reference replaced         |
| `onCircular(source)` | Each reference in a cycle                                 |
| `onBundle(reference, value, parent, key)` | Each member with a `$ref` a bundle looked through |
| `continueOnError` | Put `null` where a reference fails, carry on, list failures in `errors` |
| `timeoutMs`       | Throw once the run takes longer                              |
| `cloneReferences` | A copy per place, except where it would hold a cycle         |

```javascript
import init, { bundleSchemaFrom, resolveSchemaFrom } from "unbinder";

await init();

const { schema } = await bundleSchemaFrom("schemas/order.json", { load: { files: "any" } });

// Loaded but not dereferenced: read and change, then dereference or bundle.
const resolved = await resolveSchemaFrom("schemas/order.json", { load: { files: "any" } });
resolved.get("#/properties/customer");
resolved.set("customer.json#/required", ["id"]);
const { schema: dereferenced } = resolved.dereference({ cloneReferences: true });
```

### Coming from json-schema-ref-parser

| json-schema-ref-parser                 | unbinder                                         |
| -------------------------------------- | ------------------------------------------------ |
| `dereference`                          | `dereferenceSchema`, `dereferenceSchemaFrom`     |
| `bundle`                               | `bundleSchema`, `bundleSchemaFrom`               |
| `resolve`, `$refs.get/exists/set/paths/values` | `resolveSchemaFrom`: `get`, `exists`, `set`, `urls`, `toJSON` |
| `parse`                                | `resolveSchemaFrom` with `external: "ignore"`, then `get("#")` |
| `dereference.circular: true / false / "ignore"` | `circular: "link"` / `"reject"` / `"ignore"` |
| `dereference.mergeKeys: false`         | `siblings: "shallow"`                            |
| `excludedPathMatcher` (all three)      | `excluded`                                       |
| `onCircular`, `onDereference`, `onBundle` | the same names                                |
| `dereference.cloneReferences`          | `cloneReferences`                                |
| `externalReferenceResolution: "root"`  | `externalBase`: the working directory            |
| `dereference.maxDepth`                 | `maxDepth`                                       |
| `dereference.preservedProperties`      | always: every sibling keyword is kept            |
| `bundle.optimizeInternalRefs: false`   | `internal: "written"`                            |
| `resolve.external: false`              | `external: "ignore"`                             |
| `resolve.http` `headers`, `timeout`, `redirects`, `withCredentials`, `safeUrlResolver` | `load` `headers`, `timeoutMs`, `redirects`, `credentials: "include"`, `safeUrlResolver` |
| `resolve.http.maxContentLength`        | `maxBytes`, all documents together               |
| `resolve.file`                         | `load: { files: "any" }`                         |
| text files as strings                  | `load: { text: true }`                           |
| custom resolvers and parsers           | a `resolve` function returning the parsed document |
| `continueOnError`, `timeoutMs`, `mutateInputSchema` | the same names                       |
| `#name` and empty `$ref` left alone    | `local: "pointers"`                              |
| `$ref` to `true` with keywords becomes `true` | `trueTarget: "replace"`                   |

Schemas cross into WebAssembly as JSON: plain data, at most 128 levels deep.
`maxBytes` bounds the full output for `dereferenceSchemaText` and the shared
objects, about the document's size, for `dereferenceSchema`.

## API Reference

Full Rust documentation is on [docs.rs](https://docs.rs/unbinder).

### Rust

| Item | Does |
| ---- | ---- |
| `dereference_schema(schema)` | Inlined copy as `Dereferenced { schema, circular }`, `circular` sorted |
| `dereference_schema_with(schema, options)` | The same with [options](#options) |
| `dereference_schema_shared(schema, options)` | [Without copying](#without-copying): `SharedSchema` is `Serialize`, `to_value` builds the tree |
| `dereference_schema_with_hooks(schema, options, hooks)` | Shared, with [hooks](#hooks) |
| `bundle_schema(schema, options)`, `bundle_schema_with_hooks` | [Bundles](#bundling) one document |
| `Documents` | `insert`, `missing`, `dereference`, `bundle`, `lookup`, `set`, `urls`, `document`, and `_with_hooks` forms |
| `Stats::of(schema)` | Counts `objects`, `arrays` and `references` |

### JavaScript

```typescript
interface Options {
    circular?: "link" | "expand" | "ignore" | "reject";
    siblings?: "deep" | "shallow";
    trueTarget?: "keep" | "replace";
    maxDepth?: number;
    maxBytes?: number;
    maxDocuments?: number;
    external?: "follow" | "ignore";
    local?: "any" | "pointers";
    externalBase?: string;
    internal?: "direct" | "written";
}

interface LoadOptions {
    sameOrigin?: boolean;
    safeUrlResolver?: boolean;
    files?: "none" | "folder" | "any";
    folder?: string;
    text?: boolean;
    headers?: Record<string, string>;
    timeoutMs?: number;
    redirects?: number;
    credentials?: "same-origin" | "include" | "omit";
}

interface RunOptions {
    timeoutMs?: number;
    continueOnError?: boolean;
    cloneReferences?: boolean;
}

interface Hooks {
    excluded?: (path: string, value: unknown) => boolean;
    onCircular?: (source: string) => void;
    onDereference?: (reference: string, value: unknown, parent: unknown, key: string | undefined) => void;
    onBundle?: (reference: unknown, value: unknown, parent: unknown, key: string | undefined) => void;
}

interface Dereferenced { schema: unknown; circular: string[] }
interface Unresolved { error: Error; source: string } // `error.kind` says which
interface Bundled { schema: unknown; errors?: Unresolved[] }
interface Stats { objects: number; arrays: number; references: number }

type Given = Options & RunOptions & Hooks;
type Result = Dereferenced & { errors?: Unresolved[] };
type Resolve = (url: string) => unknown;

function dereferenceSchema(schema: any, options?: Given & { mutateInputSchema?: boolean }): Result;
function dereferenceSchemaFrom(url: string, options?: Given & { load?: LoadOptions }, resolve?: Resolve): Promise<Result>;
function bundleSchema(schema: any, options?: Given & { mutateInputSchema?: boolean }): Bundled;
function bundleSchemaFrom(url: string, options?: Given & { load?: LoadOptions }, resolve?: Resolve): Promise<Bundled>;
function resolveSchemaFrom(url: string, options?: Options & { load?: LoadOptions }, resolve?: Resolve): Promise<Resolved>;
function dereferenceSchemaText(json: string, options?: Options): string;
function schemaStats(schema: any): Stats;

class Resolved {
    readonly url: string;
    urls(): string[];
    get(reference: string): unknown;
    exists(reference: string): boolean;
    set(reference: string, value: unknown): void;
    toJSON(): Record<string, unknown>;
    dereference(options?: Given): Result;
    bundle(options?: Given): Bundled;
}
```

## Building from Source

[pixi](https://pixi.sh) pins rustup, Node.js, Bun, wasm-pack, wasm-bindgen and
wasm-opt in `pixi.lock`; `rust-toolchain.toml` pins Rust.

```bash
pixi run lint          # clippy
pixi run test:rust
pixi run build:no-std  # builds without the standard library
pixi run test:js       # builds pkg/ and tests it
pixi run verify        # all of the above, formatting and the WebAssembly lint
pixi run publish:npm
pixi run publish:jsr
```

## Benchmark

~4.3x faster than
[json-schema-ref-parser](https://github.com/APIDevTools/json-schema-ref-parser).
Run it with `pixi run bench`.

## Feature Flags

-   `std` (default): the standard library; without it, `no_std` with `alloc`.
-   `preserve_order`: keep members in document order rather than sorted.
    Always on in the JavaScript package.
-   `wasm`: the JavaScript bindings, off so Rust programs targeting
    WebAssembly don't export them.
-   `yaml`: YAML in `dereferenceSchemaFrom`. On in the published package; it
    is most of the module, which is a third the size without it.

## Limitations

-   `dereference_schema` and `dereferenceSchema` take one document and report
    a reference to another as an error; use `Documents` or the `*From`
    functions.
-   A circular reference left in place in a loaded document stays relative to
    that document, not the output.
-   A pointer through a `$ref` with siblings sees the siblings, then the
    target, never the deep merge of both.
-   Siblings of a `$ref` to `true` are kept (see `true_target`); to `false` or
    a non-schema they are dropped.
-   `mutateInputSchema` reuses only the root object; values JSON can't hold
    are lost.
-   Any object with a string `$ref`, even inside `enum`, `const`, `default` or
    `examples`, is a reference unless `excluded` says otherwise.
-   An unresolvable reference fails only where it is reached.
-   `Resolved.get` returns a copy; change documents with `set`.
-   Binary files aren't read; json-schema-ref-parser inlines a `Buffer`.
-   An array target with sibling keywords stays an array;
    json-schema-ref-parser makes an object keyed by position.
-   A reference under the name `$ref` pointing at a string is an error, since
    inlining it would leave an unresolved `$ref`.
-   `$anchor` and `$dynamicRef` aren't interpreted.
-   Dereferencing a recursive schema twice expands its cycles a second level.

## Upgrading from 0.1

-   `dereference_schema` borrows the document and returns `Dereferenced`
    instead of rewriting it; options go to `dereference_schema_with`.
-   Circular references stay as `$ref` instead of becoming `null`.
-   `Options::on_circular` is now `Dereferenced::circular`;
    `Options::resolve_external` is gone.
-   `Error` is an enum, not a `String`.
-   `count` is now `Stats::of`.
-   The `parallel`, `wee_alloc`, `console_log`, `console_error_panic_hook`,
    `text-interface` and `dev` features are gone; `dereferenceSchemaText` is
    part of `wasm`.
-   JavaScript: `dereferenceSchema` returns `{ schema, circular }`, options
    are a plain object, errors are `Error` not `WasmError`,
    `estimateSchemaSize` is `schemaStats`, and `getMemoryUsage` is gone.
-   JavaScript: references to one target share an object and cycles are
    linked; for independent copies use `cloneReferences`,
    `circular: "expand"` with `structuredClone`, or `dereferenceSchemaText`.

## Contributing

Pull requests are welcome.

## License

MIT; see [LICENSE](LICENSE).

## Author

Jakob Lochinski

## See Also

-   [JSON Schema Specification](https://json-schema.org/)
-   [JSON Pointer (RFC 6901)](https://datatracker.ietf.org/doc/html/rfc6901)
