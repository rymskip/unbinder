//! JavaScript bindings, exported when the crate is built for WebAssembly.
//!
//! Documents and options cross the boundary as JSON text. The JSON parser
//! bounds how deeply a document may nest before any of it is read, and the
//! JavaScript engine rejects an object that contains itself.
//!
//! A result returned as objects crosses as a table: each inlined target is
//! written once, with the places that hold it listed beside it, and a small
//! linker on the JavaScript side points every such place at the one object.
//! The work is then in proportion to the document, not to its expansion.
//!
//! The TypeScript declarations are generated from the Rust types, and the
//! documentation here is what a JavaScript caller reads.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use js_sys::{Function, JSON, Promise};
use serde::Serialize;
use serde_json::Value;
use tsify::Ts;
use wasm_bindgen::prelude::{JsError, JsValue, wasm_bindgen};
use wasm_bindgen_futures::JsFuture;

use crate::dereference::{Form, expand};
use crate::documents::Document;
use crate::shared::{Anchor, Shared};
use crate::{
    Dereferenced, Documents, Error, Options, SharedSchema, Stats, dereference_schema_shared,
};

#[wasm_bindgen(inline_js = r#"
// Builds the result from a table of entries. An entry is a value with holes
// and the links that fill them: [path, entry, inherit]. A plain link puts the
// value of another entry at the path, which is how a cycle closes. An
// inheriting link gives the object at the path every member of the other
// entry's value that it does not have.
export function link(text) {
  const { nodes, root, circular } = JSON.parse(text);
  // Defining a property keeps a member named __proto__ a plain member.
  const define = (holder, name, value) =>
    Object.defineProperty(holder, name, {
      value,
      writable: true,
      enumerable: true,
      configurable: true,
    });
  const place = (node, path) => {
    let holder = node;
    let slot = 0;
    for (const step of path) {
      holder = holder[slot];
      slot = step;
    }
    return [holder, slot];
  };
  // Every hole is filled before anything is inherited, so that what is
  // inherited is complete.
  for (const node of nodes) {
    for (const [path, entry, inherit] of node[1]) {
      if (!inherit) {
        const [holder, slot] = place(node, path);
        define(holder, slot, nodes[entry][0]);
      }
    }
  }
  // An entry inherits after the entries it inherits from have. The entries
  // still waiting are kept in a list, so a long chain of them is not a deep
  // call. 0 is untouched, 1 is waiting on others, 2 is done.
  const state = new Uint8Array(nodes.length);
  for (let first = 0; first < nodes.length; first += 1) {
    const waiting = [first];
    while (waiting.length > 0) {
      const index = waiting[waiting.length - 1];
      const links = nodes[index][1];
      if (state[index] === 0) {
        state[index] = 1;
        for (const [, entry, inherit] of links) {
          if (inherit && state[entry] === 0) {
            waiting.push(entry);
          }
        }
        continue;
      }
      waiting.pop();
      if (state[index] === 2) {
        continue;
      }
      state[index] = 2;
      for (const [path, entry, inherit] of links) {
        if (inherit) {
          const [holder, slot] = place(nodes[index], path);
          const heir = holder[slot];
          const source = nodes[entry][0];
          for (const name of Object.keys(source)) {
            if (!Object.hasOwn(heir, name)) {
              define(heir, name, source[name]);
            }
          }
        }
      }
    }
  }
  return { schema: nodes[root][0], circular };
}

const privateSuffixes = [
  ".localhost",
  ".local",
  ".internal",
  ".intranet",
  ".corp",
  ".home",
  ".lan",
];

// Why the default loader will not fetch `url`, or nothing if it will.
//
// A document names the URLs it refers to, so what a document can make the
// loader fetch has to be bounded. It fetches over HTTP(S) only, never from a
// host named as this machine or a private network, and never from another
// origin than the document it was asked for. The last rule is what holds
// when a public name leads to a private address: the loader contacts no
// host the caller did not name.
function refusal(url, root) {
  const { protocol, hostname, origin } = new URL(url);
  if (protocol !== "http:" && protocol !== "https:") {
    return "is not an http or https URL";
  }
  const host = hostname.replace(/\.$/, "");
  if (
    !host.includes(".") ||
    host.startsWith("[") ||
    privateSuffixes.some((suffix) => host.endsWith(suffix))
  ) {
    return "names a local or private host";
  }
  const address = host.split(".").map(Number);
  if (address.length === 4 && !address.some(Number.isNaN)) {
    const [first, second] = address;
    if (
      first === 0 ||
      first === 10 ||
      first === 127 ||
      first >= 224 ||
      (first === 100 && second >= 64 && second <= 127) ||
      (first === 169 && second === 254) ||
      (first === 172 && second >= 16 && second <= 31) ||
      (first === 192 && second === 168)
    ) {
      return "names a local or private host";
    }
  }
  if (origin !== new URL(root).origin) {
    return `is on another origin than ${root}`;
  }
  return undefined;
}

// Gathers the pieces `next` hands out into text, giving up once they pass
// `limit` bytes. `next` resolves to nothing when there are no more.
async function gather(next, limit, url) {
  const decoder = new TextDecoder();
  let text = "";
  let bytes = 0;
  for (;;) {
    const piece = await next();
    if (piece === undefined) {
      return text + decoder.decode();
    }
    bytes += piece.byteLength;
    if (bytes > limit) {
      throw new Error(`${url} is larger than the ${limit} bytes left to load`);
    }
    text += decoder.decode(piece, { stream: true });
  }
}

// Reads a response as text of at most `limit` bytes.
async function read(response, limit, url) {
  if (response.body === null) {
    return "";
  }
  const reader = response.body.getReader();
  try {
    return await gather(
      async () => {
        const { done, value } = await reader.read();
        return done ? undefined : value;
      },
      limit,
      url,
    );
  } finally {
    await reader.cancel();
  }
}

// One of the modules a runtime with a file system comes with. A browser has
// none, and a bundler has nothing to resolve, since nothing is imported.
function builtin(name) {
  const found = globalThis.process?.getBuiltinModule?.(name);
  if (found === undefined) {
    throw new Error("this runtime has no file system; pass a resolver to load files");
  }
  return found;
}

// The file: URL of the file at `location`, which is a path or such a URL
// already.
export function fileUrl(location) {
  return /^file:/i.test(location)
    ? location
    : builtin("node:url").pathToFileURL(location).href;
}

// The folder documents may be read from when the caller asked for the file
// at the URL `root`: the one the caller names, by path or file: URL, and
// otherwise the one that holds the file.
export function fileFolder(root, folder) {
  const { fileURLToPath } = builtin("node:url");
  const { dirname, resolve } = builtin("node:path");
  if (folder === undefined) {
    return dirname(fileURLToPath(root));
  }
  return /^file:/i.test(folder) ? fileURLToPath(folder) : resolve(folder);
}

const documentTypes = [".json", ".yaml", ".yml"];

// Reads the file at `url` as text of at most `limit` bytes. `root` is the
// URL of the file the caller asked for.
//
// A document names the files it refers to, so what a document can make this
// read has to be bounded. It reads files on this machine only, and only in
// `folder` or below it. The folder is checked before the disk is touched, so
// no other machine is contacted, and again once links are followed, so a
// link cannot lead out of it. A file a document refers to has to be named as
// JSON or YAML, since any text reads as YAML and would be inlined.
export async function loadFile(url, limit, root, folder) {
  const { fileURLToPath } = builtin("node:url");
  const { extname, isAbsolute, relative, sep } = builtin("node:path");
  const { open, realpath } = builtin("node:fs/promises");
  const refuse = (reason) =>
    new Error(`the file loader does not read ${url}, which ${reason}; pass a resolver to load it`);
  const { protocol, hostname } = new URL(url);
  if (protocol !== "file:") {
    throw refuse("is not a file");
  }
  if (hostname !== "") {
    throw refuse("names another machine");
  }
  const inside = (path, holder) => {
    const from = relative(holder, path);
    return from !== "" && from !== ".." && !from.startsWith(`..${sep}`) && !isAbsolute(from);
  };
  const path = fileURLToPath(url);
  if (!inside(path, folder)) {
    throw refuse(`is outside ${folder}`);
  }
  const real = await realpath(path);
  if (!inside(real, await realpath(folder))) {
    throw refuse(`is a link that leads outside ${folder}`);
  }
  if (url !== root && !documentTypes.includes(extname(real).toLowerCase())) {
    throw refuse(`is not a ${documentTypes.join(", ")} file`);
  }
  const handle = await open(real, "r");
  try {
    // Read in pieces, since what a path names need not have an end.
    const buffer = new Uint8Array(65536);
    return await gather(
      async () => {
        const { bytesRead } = await handle.read(buffer, 0, buffer.length, null);
        return bytesRead === 0 ? undefined : buffer.subarray(0, bytesRead);
      },
      limit,
      url,
    );
  } finally {
    await handle.close();
  }
}

// Loads a document the way a caller who gives no other way gets: fetched as
// text of at most `limit` bytes. `root` is the URL the caller asked for. A
// redirect is followed only where its target can be seen and passes the same
// checks.
export async function load(url, limit, root) {
  let target = url;
  for (let hops = 0; hops <= 5; hops += 1) {
    const refused = refusal(target, root);
    if (refused !== undefined) {
      throw new Error(
        `the default loader does not fetch ${target}, which ${refused}; pass a resolver to load it`,
      );
    }
    const response = await fetch(target, { redirect: "manual" });
    const location = response.headers.get("location");
    if (response.status >= 300 && response.status < 400 && location) {
      target = new URL(location, target).href;
      continue;
    }
    if (response.type === "opaqueredirect") {
      throw new Error(
        `${target} redirects somewhere the default loader cannot check; pass a resolver to load it`,
      );
    }
    if (!response.ok) {
      throw new Error(`${target} answered ${response.status}`);
    }
    return read(response, limit, target);
  }
  throw new Error(`${url} redirects too many times`);
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn link(table: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch)]
    fn load(url: &str, limit: usize, root: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_name = loadFile)]
    fn load_file(url: &str, limit: usize, root: &str, folder: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_name = fileFolder)]
    fn file_folder(root: &str, folder: Option<String>) -> Result<String, JsValue>;

    #[wasm_bindgen(catch, js_name = fileUrl)]
    fn file_url(location: &str) -> Result<String, JsValue>;
}

/// Where the documents a schema refers to come from.
enum Source {
    /// Whatever the caller's function returns for a URL.
    Resolver(Function),
    Fetched,
    /// Files in the folder at this path and below it.
    Files(String),
}

/// Replaces every `$ref` in `schema` with the value it points at.
///
/// The places that refer to one target share one object, so changing it in
/// one place changes it in all of them. `schema` itself is left untouched.
///
/// # Errors
///
/// Throws an `Error` when `schema` or `options` cannot be read, when a
/// reference cannot be resolved, and when the result exceeds a limit.
#[wasm_bindgen(js_name = dereferenceSchema)]
pub fn dereference_schema(
    schema: &JsValue,
    options: Option<Ts<Options>>,
) -> Result<Ts<Dereferenced>, JsValue> {
    let options = read_options(options)?;
    let schema = serde_json::from_str(&json_of(schema)?).map_err(thrown)?;
    let root = Document {
        root: &schema,
        url: None,
    };
    linked(&root, None, &options)
}

/// Loads the document at `url` and every document its references lead to,
/// then replaces every `$ref` in it with the value it points at, as
/// `dereferenceSchema` does.
///
/// `resolve` is called with the absolute URL of each document and returns
/// the document or a promise of it. References in a document are relative to
/// its own URL.
///
/// Without `resolve`, each document is fetched and read as JSON, or as YAML
/// in a build that has it, under rules that
/// bound what a document can make it fetch: only `http:` and `https:` URLs,
/// never one that names this machine or a private network, and only from the
/// origin of `url`. Redirects are held to the same rules. Pass a `resolve`
/// to load anything else, and use `dereferenceSchemaFromFile` for a document
/// on disk.
///
/// # Errors
///
/// Rejects when a document cannot be loaded or read, when the documents
/// together are larger than `maxBytes` or more than `maxDocuments`, and for
/// every reason `dereferenceSchema` throws.
#[wasm_bindgen(js_name = dereferenceSchemaFrom)]
pub async fn dereference_schema_from(
    url: String,
    #[wasm_bindgen(unchecked_optional_param_type = "Options")] options: Option<Ts<Options>>,
    #[wasm_bindgen(unchecked_optional_param_type = "(url: string) => unknown")] resolve: Option<
        Function,
    >,
) -> Result<Ts<Dereferenced>, JsValue> {
    let source = match resolve {
        Some(resolve) => Source::Resolver(resolve),
        None => Source::Fetched,
    };
    gathered(&url, &read_options(options)?, &source).await
}

/// `dereferenceSchemaFrom` for a document on disk, in a runtime that has a
/// file system. `path` is the path of a file or its `file:` URL.
///
/// Each document is read as JSON, or as YAML in a build that has it. A
/// document can make this read only `.json`, `.yaml` and `.yml` files on
/// this machine, in `folder` and below it, and nothing over a network.
/// `folder` is a path or a `file:` URL, and is the folder that holds `path`
/// when it is left out.
///
/// # Errors
///
/// Rejects where there is no file system, when a file cannot be read or is
/// one this does not read, and for every reason `dereferenceSchemaFrom`
/// rejects.
#[wasm_bindgen(js_name = dereferenceSchemaFromFile)]
pub async fn dereference_schema_from_file(
    path: String,
    #[wasm_bindgen(unchecked_optional_param_type = "Options")] options: Option<Ts<Options>>,
    #[wasm_bindgen(unchecked_optional_param_type = "string")] folder: Option<String>,
) -> Result<Ts<Dereferenced>, JsValue> {
    let root = file_url(&path)?;
    let source = Source::Files(file_folder(&root, folder)?);
    gathered(&root, &read_options(options)?, &source).await
}

/// Loads the document at `url` and every document it leads to from `source`,
/// then dereferences it.
async fn gathered(
    url: &str,
    options: &Options,
    source: &Source,
) -> Result<Ts<Dereferenced>, JsValue> {
    let mut documents = Documents::new();
    let mut loaded: usize = 0;
    // What the documents may still take between them.
    let mut bytes_left = options.max_bytes;
    let root = Documents::normalized(url).map_err(thrown)?;
    let mut asked = BTreeSet::from([root.clone()]);
    let mut wanted = vec![root.clone()];
    while let Some(next) = wanted.pop() {
        if loaded >= options.max_documents {
            return Err(thrown(Error::DocumentLimitExceeded {
                limit: options.max_documents,
            }));
        }
        // One load per turn, so the count cannot saturate.
        loaded = loaded.saturating_add(1);
        let text = match source {
            Source::Resolver(resolve) => {
                let answer = resolve.call1(&JsValue::NULL, &JsValue::from_str(&next))?;
                json_of(&JsFuture::from(Promise::resolve(&answer)).await?)?
            }
            // The loaders stop reading at what is left, so a document too
            // large is never held in full.
            Source::Fetched => text_of(&load(&next, bytes_left, &root)?).await?,
            Source::Files(folder) => text_of(&load_file(&next, bytes_left, &root, folder)?).await?,
        };
        bytes_left = bytes_left.checked_sub(text.len()).ok_or_else(|| {
            thrown(Error::SizeLimitExceeded {
                limit: options.max_bytes,
            })
        })?;
        let document = match source {
            Source::Resolver(_) => serde_json::from_str(&text).map_err(thrown)?,
            Source::Fetched | Source::Files(_) => read(&text, options)?,
        };
        // Only what this document refers to is new, so only that is looked
        // at, however many documents came before.
        for referred in documents.add(&next, document).map_err(thrown)? {
            if asked.insert(referred.clone()) {
                wanted.push(referred.clone());
            }
        }
    }
    let Some(document) = documents.get(&root) else {
        return Err(thrown(Error::MissingDocument { url: root }));
    };
    linked(&document, Some(&documents), options)
}

/// The text a loader promised.
async fn text_of(promised: &JsValue) -> Result<String, JsValue> {
    JsFuture::from(Promise::resolve(promised))
        .await?
        .as_string()
        .ok_or_else(|| JsError::new("the loader did not return text").into())
}

/// Dereferences `root` and hands the result to JavaScript as shared objects.
fn linked(
    root: &Document<'_>,
    documents: Option<&Documents>,
    options: &Options,
) -> Result<Ts<Dereferenced>, JsValue> {
    // The limit is on what the result takes, and shared objects take no more
    // room for being referred to often. The table counts what it writes.
    let expanded = expand(root, documents, options, Form::Linked).map_err(thrown)?;
    let table = Table::write(&expanded.schema, options)?;
    // The links in the schema have been followed, so what they point at can
    // go.
    drop(expanded.anchors);
    Ok(Ts::new_unchecked(link(&table)?))
}

/// `dereferenceSchema` for a document held as JSON text. Returns the result
/// as JSON text, in which every reference holds its own copy of its target.
///
/// # Errors
///
/// Throws an `Error` when `json` is not JSON, and for every reason
/// `dereferenceSchema` does.
#[wasm_bindgen(js_name = dereferenceSchemaText)]
pub fn dereference_schema_text(
    json: &str,
    options: Option<Ts<Options>>,
) -> Result<String, JsValue> {
    let options = read_options(options)?;
    let schema = serde_json::from_str(json).map_err(thrown)?;
    let shared = dereference_schema_shared(&schema, &options).map_err(thrown)?;
    serde_json::to_string(&Written {
        schema: &shared,
        circular: shared.circular(),
    })
    .map_err(thrown)
}

/// Counts the objects, arrays and references in `schema`.
///
/// # Errors
///
/// Throws an `Error` when `schema` cannot be read.
#[wasm_bindgen(js_name = schemaStats)]
pub fn schema_stats(schema: &JsValue) -> Result<Ts<Stats>, JsValue> {
    let schema: Value = serde_json::from_str(&json_of(schema)?).map_err(thrown)?;
    let stats = serde_json::to_string(&Stats::of(&schema)).map_err(thrown)?;
    Ok(Ts::new_unchecked(JSON::parse(&stats)?))
}

/// Reads a document the default loader fetched, which is JSON.
#[cfg(not(feature = "yaml"))]
fn read(text: &str, _: &Options) -> Result<Value, JsValue> {
    serde_json::from_str(text).map_err(thrown)
}

/// Reads a document the default loader fetched, which is JSON or YAML.
#[cfg(feature = "yaml")]
fn read(text: &str, options: &Options) -> Result<Value, JsValue> {
    let json = match serde_json::from_str(text) {
        Ok(document) => return Ok(document),
        Err(error) => error,
    };
    // A document has no more values than it has bytes, so the YAML reader's
    // limits follow the text, which is itself limited.
    let limits = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_depth: options.max_depth,
            max_nodes: text.len(),
            max_events: text.len().saturating_mul(2),
            max_total_scalar_bytes: text.len(),
        },
    };
    match serde_saphyr::from_str_with_options(text, limits) {
        Ok(document) => Ok(document),
        // Text that opens like JSON was meant as JSON, so that reader's
        // complaint is the one to pass on.
        Err(_) if text.trim_start().starts_with(['{', '[']) => Err(thrown(json)),
        Err(error) => Err(thrown(error)),
    }
}

/// The result as it is written to JSON text.
#[derive(Serialize)]
struct Written<'run, 'doc> {
    schema: &'run SharedSchema<'doc>,
    circular: &'run [String],
}

fn read_options(options: Option<Ts<Options>>) -> Result<Options, JsValue> {
    match options {
        Some(options) => serde_json::from_str(&json_of(&options.js_value())?).map_err(thrown),
        None => Ok(Options::default()),
    }
}

fn json_of(value: &JsValue) -> Result<String, JsValue> {
    JSON::stringify(value)?
        .as_string()
        .ok_or_else(|| JsError::new("value has no JSON representation").into())
}

fn thrown(error: impl core::error::Error) -> JsValue {
    JsError::from(error).into()
}

/// Why the table could not be written.
enum Failure {
    /// The result takes more than the limit allows.
    Limit(Error),
    Json(serde_json::Error),
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<Failure> for JsValue {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Limit(error) => thrown(error),
            Failure::Json(error) => thrown(error),
        }
    }
}

/// A step from a value to one of its children.
#[derive(Clone, Copy, Serialize)]
#[serde(untagged)]
enum Step<'doc> {
    Name(&'doc str),
    Position(usize),
}

/// A place in an entry that takes the value of another entry: the path to
/// it, the entry, and whether the place inherits the entry's members instead
/// of becoming it.
#[derive(Serialize)]
struct Link<'doc>(Vec<Step<'doc>>, usize, bool);

/// The memory one step of a path, or one member an object is given, takes on
/// the JavaScript side.
const SLOT_BYTES: usize = 2 * size_of::<usize>();

/// The entry of an inlined target.
#[derive(Clone, Copy)]
struct Target {
    position: usize,
    /// How many members a place that inherits from the target is given.
    members: usize,
}

/// The table the linker reads.
struct Table<'doc, 'options> {
    options: &'options Options,
    /// What the result may still take on the JavaScript side. The objects
    /// are shared, but each place that merges keywords into a target gets
    /// members of its own, so the table counts what it writes.
    bytes_left: usize,
    /// The entries written so far, by position.
    entries: BTreeMap<usize, Vec<u8>>,
    /// How many positions have been given out.
    count: usize,
    targets: BTreeMap<*const Anchor<'doc>, Target>,
    /// The targets given a position whose entries are still to be written.
    pending: Vec<(usize, Rc<Anchor<'doc>>)>,
}

impl<'doc, 'options> Table<'doc, 'options> {
    fn write(schema: &SharedSchema<'doc>, options: &'options Options) -> Result<String, Failure> {
        let mut table = Self {
            options,
            bytes_left: options.max_bytes,
            entries: BTreeMap::new(),
            count: 0,
            targets: BTreeMap::new(),
            pending: Vec::new(),
        };
        let root = table.reserve();
        table.entry(root, schema.shared())?;
        // An entry is written on its own and not inside the one that links
        // to it, so a long chain of links is not a deep call.
        while let Some((position, target)) = table.pending.pop() {
            table.entry(position, &target.value)?;
        }
        let mut text = Vec::from(*b"{\"nodes\":[");
        for (position, entry) in &table.entries {
            if *position > 0 {
                text.push(b',');
            }
            text.extend(entry);
        }
        text.extend(b"],\"root\":");
        serde_json::to_writer(&mut text, &root)?;
        text.extend(b",\"circular\":");
        serde_json::to_writer(&mut text, schema.circular())?;
        text.push(b'}');
        Ok(String::from_utf8(text).map_err(<serde_json::Error as serde::ser::Error>::custom)?)
    }

    fn charge(&mut self, bytes: usize) -> Result<(), Failure> {
        match self.bytes_left.checked_sub(bytes) {
            Some(left) => {
                self.bytes_left = left;
                Ok(())
            }
            None => Err(Failure::Limit(Error::SizeLimitExceeded {
                limit: self.options.max_bytes,
            })),
        }
    }

    /// Gives out the next position.
    fn reserve(&mut self) -> usize {
        let position = self.count;
        // One position per target, so the count cannot saturate.
        self.count = self.count.saturating_add(1);
        position
    }

    /// Writes the entry at `position` for `value`.
    fn entry(&mut self, position: usize, value: &Shared<'doc>) -> Result<(), Failure> {
        let mut entry = Entry {
            table: self,
            plain: Vec::new(),
            path: Vec::new(),
            links: Vec::new(),
        };
        entry.value(value)?;
        let Entry { plain, links, .. } = entry;
        let mut written = Vec::from(*b"[");
        written.extend(plain);
        written.push(b',');
        serde_json::to_writer(&mut written, &links)?;
        written.push(b']');
        self.entries.insert(position, written);
        Ok(())
    }

    /// The entry of an inlined target. It is given a position on first use
    /// and written later.
    fn inlined(&mut self, target: &Rc<Anchor<'doc>>) -> Target {
        if let Some(known) = self.targets.get(&Rc::as_ptr(target)) {
            return *known;
        }
        let reserved = Target {
            position: self.reserve(),
            members: target.value.width(),
        };
        self.targets.insert(Rc::as_ptr(target), reserved);
        self.pending.push((reserved.position, Rc::clone(target)));
        reserved
    }
}

/// One entry of the table as it is written.
struct Entry<'table, 'doc, 'options> {
    table: &'table mut Table<'doc, 'options>,
    /// The value, with `null` wherever a link fills it in.
    plain: Vec<u8>,
    /// The path to the value being written.
    path: Vec<Step<'doc>>,
    links: Vec<Link<'doc>>,
}

impl<'doc> Entry<'_, 'doc, '_> {
    /// Serializes `value` into the entry and counts what it takes.
    fn write(&mut self, value: &impl Serialize) -> Result<(), Failure> {
        let before = self.plain.len();
        serde_json::to_writer(&mut self.plain, value)?;
        self.table.charge(self.plain.len().saturating_sub(before))
    }

    /// Records that the place being written takes `target`.
    fn link(&mut self, target: &Rc<Anchor<'doc>>, inherit: bool) -> Result<(), Failure> {
        let target = self.table.inlined(target);
        let given = if inherit { target.members } else { 1 };
        self.table.charge(
            self.path
                .len()
                .saturating_add(given)
                .saturating_mul(SLOT_BYTES),
        )?;
        self.links
            .push(Link(self.path.clone(), target.position, inherit));
        Ok(())
    }

    fn value(&mut self, value: &Shared<'doc>) -> Result<(), Failure> {
        match value {
            Shared::Original(original) => self.write(original),
            Shared::Array(items) => {
                self.plain.push(b'[');
                for (position, item) in items.iter().enumerate() {
                    if position > 0 {
                        self.plain.push(b',');
                    }
                    self.path.push(Step::Position(position));
                    self.value(item)?;
                    self.path.pop();
                }
                self.plain.push(b']');
                Ok(())
            }
            Shared::Object(members) => {
                self.plain.push(b'{');
                let mut first = true;
                for (name, member) in members.iter() {
                    self.member(&mut first, name, member)?;
                }
                self.plain.push(b'}');
                Ok(())
            }
            Shared::Inlined(target) => {
                self.link(target, false)?;
                self.plain.extend(b"null");
                Ok(())
            }
            Shared::Back(back) => match back.target() {
                Some(target) => {
                    self.link(&target, false)?;
                    self.plain.extend(b"null");
                    Ok(())
                }
                None => self.write(back.written),
            },
            Shared::Merged(merged) => {
                self.plain.push(b'{');
                let mut first = true;
                if let Shared::Inlined(target) = &merged.base {
                    // The object takes the target's members from the one
                    // shared object, so only its own are written here.
                    self.link(target, true)?;
                    for (name, member) in &merged.overrides {
                        self.member(&mut first, name, member)?;
                    }
                } else {
                    value.members(&mut |name, member| self.member(&mut first, name, member))?;
                }
                self.plain.push(b'}');
                Ok(())
            }
        }
    }

    fn member(
        &mut self,
        first: &mut bool,
        name: &'doc str,
        member: &Shared<'doc>,
    ) -> Result<(), Failure> {
        if !*first {
            self.plain.push(b',');
        }
        *first = false;
        self.write(&name)?;
        self.plain.push(b':');
        self.path.push(Step::Name(name));
        self.value(member)?;
        self.path.pop();
        Ok(())
    }
}
