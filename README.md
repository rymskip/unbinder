# Unbinder

A high-performance JSON Schema dereferencer written in Rust with WebAssembly
support. Unbinder resolves `$ref` pointers in JSON Schema documents, making them
self-contained by replacing references with their actual content.

## Features

-   **Complete**: inlines every internal reference, including references
    inside inlined targets and references to other references
-   **Fast**: ~4.3x faster than json-schema-ref-parser
-   **Same on both sides**: a Rust server runs the crate natively and the
    frontend runs it as WebAssembly, so both validate against exactly the
    same dereferenced schema
-   **Finite on recursive schemas**: a cycle is expanded one level, then left
    as a `$ref`, and the references involved are reported
-   **Standards-based pointers**: JSON Pointer per RFC 6901, with `~0` and `~1`
    escapes and percent-encoding, and a name whose `/` was left unescaped is
    still found
-   **Bounded**: limits on output depth and size stop a hostile document from
    exhausting the stack or memory, and the analysis is linear in the document
-   **Typed errors**: every failure names the reference and the reason
-   **Portable**: `no_std` with `alloc`, no `unsafe`, and two direct
    dependencies
-   **WebAssembly bindings**: a package for npm and JSR with generated
    TypeScript types

## Installation

### As a Rust Crate

```toml
[dependencies]
unbinder = "0.2"
```

Without the standard library:

```toml
[dependencies]
unbinder = { version = "0.2", default-features = false }
```

### As a JavaScript Package

```bash
npm install unbinder
# or, from JSR
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

The input is borrowed and left untouched. Keywords next to a `$ref` are merged
over the inlined target, so a sibling `description` overrides the target's. A
pointer that reaches another `$ref` on its way passes through it.

The behaviour follows
[json-schema-ref-parser](https://github.com/APIDevTools/json-schema-ref-parser)
where a JSON document can express it. Every choice it makes for you is a
default in `Options`, which `dereference_schema_with` takes as a second
argument.

### Without copying

`dereference_schema` returns a tree, in which every reference holds its own
copy of its target. A document with many references to large targets makes
that tree far larger than the document. `dereference_schema_shared` returns a
form that holds each target once and borrows the rest from the document. It
serializes straight to the full output, as JSON or any other serde format,
without the tree ever being built.

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

A reference may point into another document, by a URL that is absolute or
relative to the document it is written in. `Documents` holds the documents by
URL and says which ones are still to load; loading them is up to you, so the
crate reads no files and opens no connections.

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

In JavaScript, `dereferenceSchemaFrom` does the loading. It fetches each
document and reads it as JSON or YAML. Its `load` option sets what it may
load, and a resolver gets the documents another way:

```javascript
import init, { dereferenceSchemaFrom } from "unbinder";

await init();

const { schema } = await dereferenceSchemaFrom(
    "https://example.com/schemas/order.json",
);

// From disk, by path or file: URL, in Node.js, Bun and Deno.
const fromDisk = await dereferenceSchemaFrom("schemas/order.json", {
    load: { files: "folder" },
});

// Everything json-schema-ref-parser loads: any public origin and any file.
const asRefParser = await dereferenceSchemaFrom("schemas/order.json", {
    load: { sameOrigin: false, files: "any", text: true },
});

// Or load documents yourself, here with a header the server asks for.
const withHeader = await dereferenceSchemaFrom(
    "https://example.com/schemas/order.json",
    {},
    async (url) =>
        (await fetch(url, { headers: { accept: "application/schema+json" } })).json(),
);
```

A document names the URLs it refers to, so by default the built-in loader
bounds what a document can make it load: only `http:` and `https:` URLs, never
one that names the local machine or a private network, only on the origin of
the URL you gave it, and no file. Redirects are held to the same rules, and it
stops reading once the documents pass `maxBytes`. `load` changes the rules:

| `load` member     | Default  | Meaning                                              |
| ----------------- | -------- | ---------------------------------------------------- |
| `sameOrigin`      | `true`   | Fetch only from the origin of the URL you gave it    |
| `safeUrlResolver` | `true`   | Refuse a host that is this machine or a private network |
| `files`           | `"none"` | `"folder"` reads documents in one folder, `"any"` reads any file |
| `folder`          |          | The folder for `"folder"`, by default the one holding the file you named |
| `text`            | `false`  | Take a document named as plain text for the string it holds |

With `sameOrigin` off, Node.js, Bun and Deno look each host up, refuse one that
leads to a private address, and connect to the address they checked. With
`files: "folder"`, a document reaches only `.json`, `.yaml` and `.yml` files on
this machine, in the folder or below it, and a link cannot lead out of it.
`files: "any"` reads whatever file a document names. With `text` on, a
document named as plain text, such as `.txt` or `.md`, is the string it holds
and is not read as JSON or YAML.

An `$id` gives a document, or a part of one, a URL of its own that references
can use, and makes pointers inside that part start at it. As in
json-schema-ref-parser, ids are followed in a document whose root has one or
names a draft of JSON Schema that defines them. Draft 4's `id` is read the
same way.

### Circular references

A tree and text cannot hold a cycle, so there a reference that takes part in
one is inlined wherever it is reached from outside the cycle and left in place
inside it. The references that close a cycle are listed in `circular`.

In JavaScript the result is made of objects, which can hold a cycle, so by
default `dereferenceSchema` links a recursive schema back into itself, as
json-schema-ref-parser does: `schema.list.properties.next === schema.list`.
Pass `circular: "expand"` for the one-level expansion instead.

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

| Field       | Default  | Meaning                                                     |
| ----------- | -------- | ----------------------------------------------------------- |
| `circular`  | `Link`   | `Link` a cycle, `Expand` it one level, `Ignore` or `Reject` it |
| `siblings`  | `Deep`   | Merge sibling objects into the target's, or replace them    |
| `true_target` | `Keep` | `Keep` the keywords next to a `$ref` to `true`, or `Replace` them with `true` |
| `max_depth` | 128      | How deep the output may nest                                |
| `max_bytes` | 256 MiB  | Roughly how much memory the output may take as a tree       |
| `max_documents` | 1024 | How many documents the references may lead through          |
| `external`  | `Follow` | `Follow` a reference to another document, or `Ignore` it and leave it as written |
| `local`     | `Any`    | Follow `Any` reference within a document, or only JSON `Pointers`, leaving `#name` and an empty `$ref` as written |
| `external_base` | none | Resolve references to other documents against this URL instead of the document they are in |
| `internal`  | `Direct` | When bundling, point a reference within the bundle `Direct`ly at its final target, or leave it as `Written` |

`Link` joins a cycle into the result where the result is JavaScript objects,
and is the same as `Expand` for a tree or text.

With `siblings: Deep`, a sibling `properties` object is merged into the
target's `properties`, and so on inward. Arrays and other values in a sibling
replace the target's. `Shallow` replaces objects too.

Writing a target out once per reference to it lets a small document describe
an enormous output. The size is worked out from the shared form, so a
document that describes more than `max_bytes` is refused before any of it is
written. Exceeding a limit returns `Error::DepthLimitExceeded` or
`Error::SizeLimitExceeded`. Dereferencing recurses once per level of depth, so
raise `max_depth` only as far as the stack allows.

### Hooks

`dereference_schema_with_hooks`, `Documents::dereference_with_hooks` and the
bundling functions take a `Hooks` implementation. Each method has a default
that changes nothing:

| Method         | Called                                              |
| -------------- | --------------------------------------------------- |
| `excluded`     | For every value reached; `true` leaves it, and everything inside it, as written |
| `circular`     | With each reference that takes part in a cycle      |
| `dereferenced` | With each reference that was replaced, and where    |
| `bundled`      | With each member a bundle looks through that has a `$ref` |
| `failed`       | With a reference that does not resolve; `Ok` carries on with `null` in its place |
| `proceed`      | Before each value, so a run can be stopped, say on a time limit |

Each is given a `Place`: its path in the output, the value as written, and
`source()`, where the value is written.

### Bundling

`bundle_schema` and `Documents::bundle` produce one document whose references
all point within it, without inlining them. Each value of another document
that a reference leads to is written once, at the place json-schema-ref-parser
would choose, and every other reference to it, or into it, points there.
References within the document are pointed straight at what they lead to.

`Documents::lookup` reads the value a URL with a JSON Pointer points at,
passing through references on the way, and `Documents::set` writes one.

### JavaScript/TypeScript

The WebAssembly module must be initialized before use.

```javascript
import init, { dereferenceSchema } from "unbinder";

await init();

const { schema, circular } = dereferenceSchema({
    type: "object",
    properties: {
        name: { $ref: "#/definitions/name" },
    },
    definitions: {
        name: { type: "string" },
    },
});

console.log(JSON.stringify(schema, null, 2));
```

Every place that refers to one target holds the same object, as with
json-schema-ref-parser, so the result takes about as much memory as the
document and changing a target in one place changes it in all of them. The
schema passed in is left untouched. `dereferenceSchemaText` returns JSON text
in which every reference has its own copy.

Options are a plain object, and failures are thrown as `Error`, with the
kind of failure in `error.detail`:

```typescript
import init, { dereferenceSchema, type Options } from "unbinder";

await init();

const options: Options = { circular: "reject", maxDepth: 64, maxBytes: 1_000_000 };
// circular: "link" | "expand" | "ignore" | "reject", siblings: "deep" | "shallow",
// trueTarget: "keep" | "replace"

try {
    dereferenceSchema({ self: { $ref: "#/self" } }, options);
} catch (error) {
    if (error instanceof Error) {
        console.error("Dereferencing failed:", error.message);
    }
}
```

## API Reference

### Rust API

Full documentation is on [docs.rs](https://docs.rs/unbinder).

#### `dereference_schema(schema: &Value) -> Result<Dereferenced, Error>`

Returns a copy of `schema` with its references inlined. `Dereferenced` holds
the result in `schema` and the references left in place, sorted, in
`circular`.

#### `dereference_schema_with(schema: &Value, options: &Options) -> Result<Dereferenced, Error>`

The same, with [options](#options) in place of the defaults.

#### `dereference_schema_shared(schema: &Value, options: &Options) -> Result<SharedSchema, Error>`

The same result [without copying](#without-copying). `SharedSchema` implements
`Serialize`, and `to_value` builds the tree from it.

#### `dereference_schema_with_hooks(schema, options, hooks) -> Result<SharedSchema, Error>`

The shared result, with [hooks](#hooks).

#### `bundle_schema(schema: &Value, options: &Options) -> Result<Value, Error>`

[Bundles](#bundling) one document. `bundle_schema_with_hooks` takes hooks.

#### `Documents`

A set of documents that refer to one another: `insert(url, document)`,
`missing()` for the URLs still to load, `dereference(url, options)`,
`bundle(url, options)`, `lookup(reference, options)`, `set(reference, value)`,
`urls()` and `document(url)`. The `_with_hooks` forms take hooks. See
[references to other documents](#references-to-other-documents).

#### `Stats::of(schema: &Value) -> Stats`

Counts the `objects`, `arrays` and `references` in a document.

### JavaScript API

```typescript
interface Options {
    circular?: "link" | "expand" | "ignore" | "reject";
    siblings?: "deep" | "shallow";
    trueTarget?: "keep" | "replace";
    maxDepth?: number;
    maxBytes?: number;
    maxDocuments?: number;
}

interface Dereferenced {
    schema: unknown;
    circular: string[];
}

interface Stats {
    objects: number;
    arrays: number;
    references: number;
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

interface Unresolved {
    error: Error; // the failure, by `kind`
    source: string;
}

interface Bundled {
    schema: unknown;
    errors?: Unresolved[];
}

type Given = Options & RunOptions & Hooks;

function dereferenceSchema(
    schema: any,
    options?: Given & { mutateInputSchema?: boolean },
): Dereferenced & { errors?: Unresolved[] };
function dereferenceSchemaFrom(
    url: string,
    options?: Given & { load?: LoadOptions },
    resolve?: (url: string) => unknown,
): Promise<Dereferenced & { errors?: Unresolved[] }>;
function bundleSchema(schema: any, options?: Given & { mutateInputSchema?: boolean }): Bundled;
function bundleSchemaFrom(
    url: string,
    options?: Given & { load?: LoadOptions },
    resolve?: (url: string) => unknown,
): Promise<Bundled>;
function resolveSchemaFrom(
    url: string,
    options?: Options & { load?: LoadOptions },
    resolve?: (url: string) => unknown,
): Promise<Resolved>;
function dereferenceSchemaText(json: string, options?: Options): string;
function schemaStats(schema: any): Stats;

class Resolved {
    readonly url: string;
    urls(): string[];
    get(reference: string): unknown;
    exists(reference: string): boolean;
    set(reference: string, value: unknown): void;
    toJSON(): Record<string, unknown>;
    dereference(options?: Given): Dereferenced & { errors?: Unresolved[] };
    bundle(options?: Given): Bundled;
}
```

Every function that dereferences or bundles also takes these members in its
options, besides `Options`:

| Member            | Meaning                                                  |
| ----------------- | -------------------------------------------------------- |
| `excluded(path, value)` | Return `true` to leave the value at `path`, and everything inside it, as written |
| `onDereference(reference, value, parent, key)` | Called with each reference replaced and what replaced it |
| `onCircular(source)` | Called with where each reference in a cycle is written |
| `onBundle(reference, value, parent, key)` | Called with each member a bundle looked through that has a `$ref` |
| `continueOnError` | Put `null` where a reference does not resolve, carry on, and list each failure in `errors` |
| `timeoutMs`       | Throw once the run has taken this long                    |
| `cloneReferences` | Give each place its own copy of its target, except where the copy would hold a cycle |

```javascript
import init, { bundleSchemaFrom, resolveSchemaFrom } from "unbinder";

await init();

// One document with every reference pointing within it.
const { schema } = await bundleSchemaFrom("schemas/order.json", {
    load: { files: "any" },
});

// The documents as loaded, to read and change before dereferencing.
const resolved = await resolveSchemaFrom("schemas/order.json", {
    load: { files: "any" },
});
resolved.get("#/properties/customer");
resolved.set("customer.json#/required", ["id"]);
const { schema: dereferenced } = resolved.dereference({ cloneReferences: true });
```

The built-in loader also takes `headers` for every request, `timeoutMs` per
request (a minute by default), `redirects` (5 by default) and
`credentials`, in `load`. Both apply only to requests to the origin of the URL
you gave, so a document cannot send a secret elsewhere by naming another
origin or redirecting to one. A `resolve` function decides for itself what
each request sends.

### What json-schema-ref-parser does, and how to get it here

| json-schema-ref-parser                 | unbinder                                         |
| -------------------------------------- | ------------------------------------------------ |
| `dereference`                          | `dereferenceSchema`, `dereferenceSchemaFrom`     |
| `bundle`                               | `bundleSchema`, `bundleSchemaFrom`               |
| `resolve`, `$refs.get/exists/set/paths/values` | `resolveSchemaFrom`, then `get`, `exists`, `set`, `urls`, `toJSON` |
| `parse`                                | `resolveSchemaFrom` with `external: "ignore"`, then `get("#")` |
| `dereference.circular: true / false / "ignore"` | `circular: "link"` / `"reject"` / `"ignore"` |
| `dereference.mergeKeys: false`         | `siblings: "shallow"`                            |
| `dereference.excludedPathMatcher`, `bundle.excludedPathMatcher`, `resolveExcludedPathMatcher` | `excluded` |
| `dereference.onCircular`, `onDereference`, `bundle.onBundle` | `onCircular`, `onDereference`, `onBundle` |
| `dereference.cloneReferences`          | `cloneReferences`                                |
| `dereference.externalReferenceResolution: "root"` | `externalBase` set to the working directory |
| `dereference.maxDepth`                 | `maxDepth`                                       |
| `dereference.preservedProperties`      | Always: every keyword next to a `$ref` is kept   |
| `bundle.optimizeInternalRefs: false`   | `internal: "written"`                            |
| `resolve.external: false`              | `external: "ignore"`                             |
| `resolve.http` `headers`, `timeout`, `redirects`, `withCredentials`, `safeUrlResolver` | `load` `headers`, `timeoutMs`, `redirects`, `credentials: "include"`, `safeUrlResolver` |
| `resolve.http.maxContentLength`        | `maxBytes`, for all documents together           |
| `resolve.file`, files by default       | `load: { files: "any" }`                          |
| Text files as strings                  | `load: { text: true }`                           |
| A custom resolver or parser            | The `resolve` function, which returns the parsed document |
| `continueOnError`                      | `continueOnError`, with the failures in `errors`  |
| `timeoutMs`                            | `timeoutMs`                                      |
| `mutateInputSchema`                    | `mutateInputSchema`                              |
| `#name` and empty `$ref` left alone    | `local: "pointers"`                              |
| A reference to `true` with keywords becomes `true` | `trueTarget: "replace"`              |

Documents cross the WebAssembly boundary as JSON, so a schema must be plain
JSON data and may nest at most 128 levels deep. For `dereferenceSchemaText`,
`maxBytes` bounds the output written out in full. For `dereferenceSchema` it
bounds what the shared objects take, which is about the size of the document
however often a target is referred to.

## Building from Source

The environment is pinned with [pixi](https://pixi.sh): `pixi.lock` fixes
rustup, Node.js, Bun, wasm-pack, wasm-bindgen and wasm-opt, and
`rust-toolchain.toml` fixes the Rust toolchain rustup installs. Every command
below runs inside that environment.

```bash
# Lint and test the crate
pixi run lint
pixi run test:rust

# Prove the crate builds without the standard library
pixi run build:no-std

# Build the JavaScript package into pkg/ and test it
pixi run test:js

# Everything above, plus formatting and the WebAssembly lint
pixi run verify

# Publish pkg/ to npm and to JSR
pixi run publish:npm
pixi run publish:jsr
```

## Benchmark

~4.3x faster than
[json-schema-ref-parser](https://github.com/APIDevTools/json-schema-ref-parser).

Run it with `pixi run bench`.

## Feature Flags

-   `std` (default): links the standard library. Without it the crate is
    `no_std` and needs only `alloc`.
-   `preserve_order`: keeps the members of every object in the order the
    document has them. Without it they come out sorted by name. The
    JavaScript package always has it.
-   `wasm`: the JavaScript bindings. Off by default, so a Rust program that
    targets WebAssembly does not export them.
-   `yaml`: lets `dereferenceSchemaFrom` read YAML documents as well as JSON.
    The published JavaScript package is built with it. The YAML parser is
    most of that module's size, so a build without it is about a third as
    large.

## Limitations

-   A reference into another document is followed only through `Documents`
    or `dereferenceSchemaFrom`. `dereference_schema` and `dereferenceSchema`
    take one document and report such a reference as an error.
-   A circular reference left in place inside a document that was loaded
    stays as written, relative to that document and not to the output.
-   A pointer that passes through a `$ref` with sibling keywords finds the
    siblings first and the target's members second. It does not see the two
    merged, so it cannot reach into an object that deep merging would combine.
-   Keywords next to a `$ref` are kept as they are when its target is the
    schema `true`, unless `true_target` says otherwise, and dropped when it is
    `false` or is not a schema.
-   `mutateInputSchema` writes the result into the object you passed and
    returns that object. The objects inside it are new ones, and a value JSON
    cannot hold is not carried over.
-   Every object with a string `$ref` member is treated as a reference,
    including one inside `enum`, `const`, `default` or `examples`, unless
    `excluded` leaves it out.
-   A reference that does not resolve fails only where it is reached, so one
    in a part of another document that nothing inlines is not reported.
-   `Resolved.get` returns a copy, so changing it changes nothing loaded;
    `set` does that.
-   A binary file is not read as bytes: json-schema-ref-parser inlines a
    `Buffer` for an image, which JSON cannot hold.
-   A target that is an array with keywords next to its `$ref` stays an array
    when dereferenced. json-schema-ref-parser turns it into an object keyed by
    position.
-   A reference written under the name `$ref` that points at a string is an
    error. Inlining it would leave a `$ref` that nothing resolved.
-   `$anchor` and `$dynamicRef` are not interpreted.
-   Dereferencing a recursive schema twice expands its cycles a second level.

## Upgrading from 0.1

-   `dereference_schema` borrows the document and returns a `Dereferenced`
    value. It no longer rewrites the document in place. Options go to
    `dereference_schema_with`.
-   A circular reference is left as a `$ref`. It is no longer replaced with
    `null`.
-   `Options::on_circular` is replaced by `Dereferenced::circular`, and
    `Options::resolve_external` is removed.
-   `Error` is an enum. It was a `String`.
-   `count` is replaced by `Stats::of`.
-   The `parallel`, `wee_alloc`, `console_log`, `console_error_panic_hook`,
    `text-interface` and `dev` features are removed. `dereferenceSchemaText`
    is part of `wasm`.
-   In JavaScript, `dereferenceSchema` returns `{ schema, circular }`, options
    are a plain object instead of an `Options` class, errors are `Error`
    instances instead of `WasmError`, `estimateSchemaSize` is replaced by
    `schemaStats`, and `getMemoryUsage` is removed.
-   In JavaScript, the places that refer to one target share one object, and
    a recursive schema is linked back into itself. Use `circular: "expand"`
    with `structuredClone`, or `dereferenceSchemaText`, for independent
    copies.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file
for details.

## Author

Jakob Lochinski

## See Also

-   [JSON Schema Specification](https://json-schema.org/)
-   [JSON Pointer (RFC 6901)](https://datatracker.ietf.org/doc/html/rfc6901)
