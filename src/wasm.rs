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

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use js_sys::JSON;
use serde::Serialize;
use serde_json::Value;
use tsify::Ts;
use wasm_bindgen::prelude::{JsError, JsValue, wasm_bindgen};

use crate::dereference::expand;
use crate::shared::Shared;
use crate::{Dereferenced, Error, Options, SharedSchema, Stats, dereference_schema_shared};

#[wasm_bindgen(inline_js = r#"
// Builds the result from a table of entries. An entry is a value with holes
// and the links that fill them: [path, entry, inherit]. A plain link puts the
// value of another entry at the path. An inheriting link gives the object at
// the path every member of the other entry's value that it does not have.
// Entries come after the entries they link to, and the document is last.
export function link(text) {
  const { nodes, circular } = JSON.parse(text);
  // Defining a property keeps a member named __proto__ a plain member.
  const define = (holder, name, value) =>
    Object.defineProperty(holder, name, {
      value,
      writable: true,
      enumerable: true,
      configurable: true,
    });
  for (const node of nodes) {
    for (const [path, entry, inherit] of node[1]) {
      let holder = node;
      let slot = 0;
      for (const step of path) {
        holder = holder[slot];
        slot = step;
      }
      const source = nodes[entry][0];
      if (inherit) {
        const heir = holder[slot];
        for (const name of Object.keys(source)) {
          if (!Object.hasOwn(heir, name)) {
            define(heir, name, source[name]);
          }
        }
      } else {
        define(holder, slot, source);
      }
    }
  }
  return { schema: nodes[nodes.length - 1][0], circular };
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn link(table: &str) -> Result<JsValue, JsValue>;
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
    // The limit is on what the result takes, and shared objects take no more
    // room for being referred to often. The table counts what it writes.
    let (shared, _) = expand(&schema, &options).map_err(thrown)?;
    let table = Table::write(&shared, &options)?;
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
    entries: Vec<u8>,
    count: usize,
    targets: BTreeMap<*const Shared<'doc>, Target>,
}

impl<'doc, 'options> Table<'doc, 'options> {
    fn write(schema: &SharedSchema<'doc>, options: &'options Options) -> Result<String, Failure> {
        let mut table = Self {
            options,
            bytes_left: options.max_bytes,
            entries: Vec::new(),
            count: 0,
            targets: BTreeMap::new(),
        };
        table.entry(schema.shared())?;
        let mut text = Vec::from(*b"{\"nodes\":[");
        text.extend(table.entries);
        text.extend(b"],\"circular\":");
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

    /// Writes the entry for `value` after the entries it links to, and
    /// returns its position.
    fn entry(&mut self, value: &Shared<'doc>) -> Result<usize, Failure> {
        let mut entry = Entry {
            table: self,
            plain: Vec::new(),
            path: Vec::new(),
            links: Vec::new(),
        };
        entry.value(value)?;
        let Entry { plain, links, .. } = entry;
        if self.count > 0 {
            self.entries.push(b',');
        }
        self.entries.push(b'[');
        self.entries.extend(plain);
        self.entries.push(b',');
        serde_json::to_writer(&mut self.entries, &links)?;
        self.entries.push(b']');
        let position = self.count;
        // One entry per target, so the count cannot saturate.
        self.count = self.count.saturating_add(1);
        Ok(position)
    }

    /// The entry of an inlined target, written on first use.
    fn inlined(&mut self, target: &Rc<Shared<'doc>>) -> Result<Target, Failure> {
        if let Some(known) = self.targets.get(&Rc::as_ptr(target)) {
            return Ok(*known);
        }
        let written = Target {
            position: self.entry(target)?,
            members: target.width(),
        };
        self.targets.insert(Rc::as_ptr(target), written);
        Ok(written)
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
    fn link(&mut self, target: &Rc<Shared<'doc>>, inherit: bool) -> Result<(), Failure> {
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
