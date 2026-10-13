# Unbinder

A high-performance JSON Schema dereferencer written in Rust with WebAssembly
support. Unbinder resolves `$ref` pointers in JSON Schema documents, making them
self-contained by replacing references with their actual content.

## Features

-   **Complete**: inlines every internal reference, including references
    inside inlined targets and references to other references
-   **Fast**: ~5.5x faster than json-schema-ref-parser
-   **Same on both sides**: a Rust server runs the crate natively and the
    frontend runs it as WebAssembly, so both validate against exactly the
    same dereferenced schema
-   **Finite on recursive schemas**: a cycle is expanded one level, then left
    as a `$ref`, and the references involved are reported
-   **Standards-based pointers**: JSON Pointer per RFC 6901, with `~0` and `~1`
    escapes and percent-encoding
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
document and reads it as JSON or YAML, unless it is given another way to get
one:

```javascript
import init, { dereferenceSchemaFrom } from "unbinder";
import { readFile } from "node:fs/promises";

await init();

const { schema } = await dereferenceSchemaFrom(
    "https://example.com/schemas/order.json",
);

// Or load documents yourself, here from disk.
const fromDisk = await dereferenceSchemaFrom(
    new URL("schemas/order.json", import.meta.url).href,
    {},
    async (url) => JSON.parse(await readFile(new URL(url), "utf8")),
);
```

A document names the URLs it refers to, so the built-in loader bounds what a
document can make it fetch: only `http:` and `https:` URLs, never one that
names the local machine or a private network, and only from the origin of the
URL you gave it. Redirects are held to the same rules, and it stops reading
once the documents pass `maxBytes`. To load anything else, pass a resolver.

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
| `max_depth` | 128      | How deep the output may nest                                |
| `max_bytes` | 256 MiB  | Roughly how much memory the output may take as a tree       |
| `max_documents` | 1024 | How many documents the references may lead through          |

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

Options are a plain object, and failures are thrown as `Error`:

```typescript
import init, { dereferenceSchema, type Options } from "unbinder";

await init();

const options: Options = { circular: "reject", maxDepth: 64, maxBytes: 1_000_000 };
// circular: "link" | "expand" | "ignore" | "reject", siblings: "deep" | "shallow"

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

#### `Documents`

A set of documents that refer to one another: `insert(url, document)`,
`missing()` for the URLs still to load, and `dereference(url, options)`. See
[references to other documents](#references-to-other-documents).

#### `Stats::of(schema: &Value) -> Stats`

Counts the `objects`, `arrays` and `references` in a document.

### JavaScript API

```typescript
interface Options {
    circular?: "link" | "expand" | "ignore" | "reject";
    siblings?: "deep" | "shallow";
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

function dereferenceSchema(schema: any, options?: Options): Dereferenced;
function dereferenceSchemaFrom(
    url: string,
    options?: Options,
    resolve?: (url: string) => unknown,
): Promise<Dereferenced>;
function dereferenceSchemaText(json: string, options?: Options): string;
function schemaStats(schema: any): Stats;
```

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

~5.5x faster than
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
    schema `true`, and dropped when it is `false` or is not a schema.
-   Every object with a string `$ref` member is treated as a reference,
    including one inside `enum`, `const`, `default` or `examples`.
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
