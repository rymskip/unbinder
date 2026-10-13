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
  const { nodes, order, root, circular } = JSON.parse(text);
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
  // An entry was finished after the entries it takes members from.
  for (const index of order) {
    const node = nodes[index];
    for (const [path, entry, inherit] of node[1]) {
      if (inherit) {
        const [holder, slot] = place(node, path);
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

// Reads a response as text, giving up once it passes `limit` bytes.
async function read(response, limit, url) {
  if (response.body === null) {
    return "";
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let text = "";
  let bytes = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      return text + decoder.decode();
    }
    bytes += value.byteLength;
    if (bytes > limit) {
      await reader.cancel();
      throw new Error(`${url} is larger than the ${limit} bytes left to load`);
    }
    text += decoder.decode(value, { stream: true });
  }
}

// Loads a document the way a caller who gives no other way gets: fetched as
// JSON text of at most `limit` bytes. `root` is the URL the caller asked
// for. A redirect is followed only where its target can be seen and passes
// the same checks.
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
    linked(root, None, &options)
}

/// Loads the document at `url` and every document its references lead to,
/// then replaces every `$ref` in it with the value it points at, as
/// `dereferenceSchema` does.
///
/// `resolve` is called with the absolute URL of each document and returns
/// the document or a promise of it. References in a document are relative to
/// its own URL.
///
/// Without `resolve`, each document is fetched as JSON, under rules that
/// bound what a document can make it fetch: only `http:` and `https:` URLs,
/// never one that names this machine or a private network, and only from the
/// origin of `url`. Redirects are held to the same rules. Pass a `resolve`
/// to load anything else.
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
    let options = read_options(options)?;
    let mut documents = Documents::new();
    let mut loaded: usize = 0;
    // What the documents may still take between them.
    let mut bytes_left = options.max_bytes;
    let root = Documents::normalized(&url).map_err(thrown)?;
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
        let text = match &resolve {
            Some(resolve) => {
                let answer = resolve.call1(&JsValue::NULL, &JsValue::from_str(&next))?;
                json_of(&JsFuture::from(Promise::resolve(&answer)).await?)?
            }
            // The default loader stops reading at what is left, so a
            // document too large is never held in full.
            None => JsFuture::from(Promise::resolve(&load(&next, bytes_left, &root)?))
                .await?
                .as_string()
                .ok_or_else(|| JsError::new("the loader did not return text"))?,
        };
        bytes_left = bytes_left.checked_sub(text.len()).ok_or_else(|| {
            thrown(Error::SizeLimitExceeded {
                limit: options.max_bytes,
            })
        })?;
        let document = serde_json::from_str(&text).map_err(thrown)?;
        // Only what this document refers to is new, so only that is looked
        // at, however many documents came before.
        for referred in documents.add(&next, document).map_err(thrown)? {
            if asked.insert(referred.clone()) {
                wanted.push(referred.clone());
            }
        }
    }
    let root = documents.find(None, &root).map_err(thrown)?;
    linked(root, Some(&documents), &options)
}

/// Dereferences `root` and hands the result to JavaScript as shared objects.
fn linked(
    root: Document<'_>,
    documents: Option<&Documents>,
    options: &Options,
) -> Result<Ts<Dereferenced>, JsValue> {
    // The limit is on what the result takes, and shared objects take no more
    // room for being referred to often. The table counts what it writes.
    let (shared, _) = expand(root, documents, options, Form::Linked).map_err(thrown)?;
    let table = Table::write(&shared, options)?;
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
    /// The positions in the order their entries were finished.
    order: Vec<usize>,
    /// How many positions have been given out.
    count: usize,
    targets: BTreeMap<*const Anchor<'doc>, Target>,
}

impl<'doc, 'options> Table<'doc, 'options> {
    fn write(schema: &SharedSchema<'doc>, options: &'options Options) -> Result<String, Failure> {
        let mut table = Self {
            options,
            bytes_left: options.max_bytes,
            entries: BTreeMap::new(),
            order: Vec::new(),
            count: 0,
            targets: BTreeMap::new(),
        };
        let root = table.reserve();
        table.entry(root, schema.shared())?;
        let mut text = Vec::from(*b"{\"nodes\":[");
        for (position, entry) in &table.entries {
            if *position > 0 {
                text.push(b',');
            }
            text.extend(entry);
        }
        text.extend(b"],\"order\":");
        serde_json::to_writer(&mut text, &table.order)?;
        text.extend(b",\"root\":");
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
        self.order.push(position);
        Ok(())
    }

    /// The entry of an inlined target, written on first use. Its position is
    /// given out before it is written, so a link back to it from inside it
    /// has somewhere to point.
    fn inlined(&mut self, target: &Rc<Anchor<'doc>>) -> Result<Target, Failure> {
        if let Some(known) = self.targets.get(&Rc::as_ptr(target)) {
            return Ok(*known);
        }
        let reserved = Target {
            position: self.reserve(),
            members: target.value.width(),
        };
        self.targets.insert(Rc::as_ptr(target), reserved);
        self.entry(reserved.position, &target.value)?;
        Ok(reserved)
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
        let target = self.table.inlined(target)?;
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
