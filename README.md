# Unbinder

A high-performance JSON Schema dereferencer written in Rust with WebAssembly
support. Unbinder resolves `$ref` pointers in JSON Schema documents, making them
self-contained by replacing references with their actual content.

## Features

-   **Complete**: inlines every internal reference, including references
    inside inlined targets and references to other references
-   **Fast on large documents**: each target is expanded once and shared, and
    the parts of the document with no reference in them are not copied, so
    the work grows with the document and not with its expansion
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
        "billing": { "$ref": "#/address" },
        "shipping": { "$ref": "#/address" },
        "address": { "type": "object" }
    });

    let shared = dereference_schema_shared(&schema, &Options::default())?;

    assert_eq!(
        serde_json::to_string(&shared)?,
        r#"{"address":{"type":"object"},"billing":{"type":"object"},"shipping":{"type":"object"}}"#
    );
    Ok(())
}
```

### Circular references

By default a reference that takes part in a cycle is inlined wherever it is
reached from outside the cycle and left in place inside it. The references
left in place are listed in `circular`.

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
| `circular`  | `Expand` | `Expand` a cycle one level, `Ignore` it, or `Reject` it     |
| `siblings`  | `Deep`   | Merge sibling objects into the target's, or replace them    |
| `max_depth` | 128      | How deep the output may nest                                |
| `max_bytes` | 256 MiB  | Roughly how much memory the output may take as a tree       |

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
// circular: "expand" | "ignore" | "reject", siblings: "deep" | "shallow"

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

#### `Stats::of(schema: &Value) -> Stats`

Counts the `objects`, `arrays` and `references` in a document.

### JavaScript API

```typescript
interface Options {
    circular?: "expand" | "ignore" | "reject";
    siblings?: "deep" | "shallow";
    maxDepth?: number;
    maxBytes?: number;
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

`pixi run bench` builds a container from the same pinned environment and
compares unbinder, as WebAssembly and as a native library, with
[json-schema-ref-parser](https://github.com/APIDevTools/json-schema-ref-parser)
on two synthetic documents and on the Kubernetes and Stripe API
descriptions. It needs Docker. `pixi run bench:run` runs the same comparison
directly on this machine.

## Feature Flags

-   `std` (default): links the standard library. Without it the crate is
    `no_std` and needs only `alloc`.
-   `wasm`: the JavaScript bindings. Off by default, so a Rust program that
    targets WebAssembly does not export them.

## Limitations

-   Only internal references (starting with `#`) are supported. An external
    reference is an error.
-   A pointer that passes through a `$ref` with sibling keywords finds the
    siblings first and the target's members second. It does not see the two
    merged, so it cannot reach into an object that deep merging would combine.
-   A sibling of a `$ref` whose target is not an object is dropped.
-   Every object with a string `$ref` member is treated as a reference,
    including one inside `enum`, `const`, `default` or `examples`.
-   `$id`, `$anchor` and `$dynamicRef` are not interpreted.
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
-   In JavaScript, the places that refer to one target share one object. Use
    `structuredClone` or `dereferenceSchemaText` for independent copies.

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
