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

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::ToString;

use js_sys::{Date, Function, JSON, Promise, Reflect};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tsify::{Ts, Tsify};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::{JsError, JsValue, wasm_bindgen};
use wasm_bindgen_futures::JsFuture;

use crate::dereference::{Form, expand};
use crate::documents::Document;
use crate::hooks::{Hooks, Place, Step, fragment};
use crate::shared::{Anchor, Shared};
use crate::walk::reference_of;
use crate::{Documents, Error, Options, SharedSchema, Stats, dereference_schema_shared};

#[wasm_bindgen(inline_js = r#"
// Builds the result from a table of entries. An entry is a value with holes
// and the links that fill them: [path, entry, inherit]. A plain link puts the
// value of another entry at the path, which is how a cycle closes. An
// inheriting link gives the object at the path every member of the other
// entry's value that it does not have.
//
// `into` is the object the caller wants rewritten in place of getting a new
// one. It takes the members of the result before any link is made, so every
// link to the result leads to it.
// Writes the members of `result` into `into`, which loses its own.
export function rewrite(into, result) {
  if (Array.isArray(into)) {
    into.length = 0;
  }
  for (const name of Object.keys(into)) {
    delete into[name];
  }
  for (const name of Object.keys(result)) {
    Object.defineProperty(into, name, {
      value: result[name],
      writable: true,
      enumerable: true,
      configurable: true,
    });
  }
}

export function link(text, into) {
  const { nodes, root, ...rest } = JSON.parse(text);
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
  // The result may be another entry whole, and that entry is then the one
  // to rewrite.
  let whole = root;
  for (let hops = 0; hops < nodes.length; hops += 1) {
    const through = nodes[whole][1].find(
      ([path, , inherit]) => path.length === 0 && !inherit,
    );
    if (through === undefined) {
      break;
    }
    whole = through[1];
  }
  const result = nodes[whole][0];
  if (
    typeof into === "object" && into !== null &&
    typeof result === "object" && result !== null &&
    Array.isArray(into) === Array.isArray(result)
  ) {
    if (Array.isArray(into)) {
      into.length = 0;
    }
    for (const name of Object.keys(into)) {
      delete into[name];
    }
    for (const name of Object.keys(result)) {
      define(into, name, result[name]);
    }
    nodes[whole][0] = into;
  }
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
  return { schema: nodes[root][0], ...rest };
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

// The networks no public host is in: this machine, private and link-local
// networks, the ranges set aside for documents and tests, and the ranges
// that carry an IPv4 address inside an IPv6 one. An IPv4 address written as
// an IPv6 one is held to the IPv4 list, so that range is not closed whole.
const closedIPv4 = [
  ["0.0.0.0", 8],
  ["10.0.0.0", 8],
  ["100.64.0.0", 10],
  ["127.0.0.0", 8],
  ["169.254.0.0", 16],
  ["172.16.0.0", 12],
  ["192.0.0.0", 24],
  ["192.0.2.0", 24],
  ["192.88.99.0", 24],
  ["192.168.0.0", 16],
  ["198.18.0.0", 15],
  ["198.51.100.0", 24],
  ["203.0.113.0", 24],
  ["224.0.0.0", 3],
];
const closedIPv6 = [
  ["::", 96],
  ["64:ff9b::", 96],
  ["64:ff9b:1::", 48],
  ["100::", 64],
  ["2001::", 23],
  ["2001:db8::", 32],
  ["2002::", 16],
  ["fc00::", 7],
  ["fe80::", 10],
  ["fec0::", 10],
  ["ff00::", 8],
];

// Whether the four parts of an IPv4 address put it in a closed network.
function closed(parts) {
  const number = (bytes) => bytes.reduce((sum, byte) => sum * 256 + byte, 0);
  return closedIPv4.some(([network, prefix]) => {
    const size = 2 ** (32 - prefix);
    return (
      Math.floor(number(parts) / size) ===
        Math.floor(number(network.split(".").map(Number)) / size)
    );
  });
}

const refused = (verb, url, reason) =>
  new Error(`the default loader does not ${verb} ${url}, which ${reason}; pass a resolver to load it`);

// Why the default loader will not fetch `url`, or nothing if it will.
//
// A document names the URLs it refers to, so what a document can make the
// loader fetch has to be bounded. It fetches over HTTP(S) only. Unless the
// caller turned the rule off, it never fetches from a host named as this
// machine or a private network, and never from another origin than the
// document it was asked for. The last rule is what holds when a public name
// leads to a private address: the loader contacts no host the caller did not
// name.
function refusal(url, root, policy) {
  const { protocol, hostname, origin } = new URL(url);
  if (protocol !== "http:" && protocol !== "https:") {
    return "is not an http or https URL";
  }
  const host = hostname.replace(/\.$/, "");
  if (policy.safeUrlResolver) {
    if (
      !host.includes(".") ||
      host.startsWith("[") ||
      privateSuffixes.some((suffix) => host.endsWith(suffix))
    ) {
      return "names a local or private host";
    }
    const address = host.split(".").map(Number);
    if (address.length === 4 && !address.some(Number.isNaN) && closed(address)) {
      return "names a local or private host";
    }
  }
  if (policy.sameOrigin && origin !== new URL(root).origin) {
    return `is on another origin than ${root}`;
  }
  return undefined;
}

// Gathers the pieces `next` hands out, giving up once they pass `limit`
// bytes. `next` resolves to nothing when there are no more, and may reuse
// the memory of a piece for the next one.
async function gather(next, limit, url) {
  const pieces = [];
  let bytes = 0;
  for (;;) {
    const piece = await next();
    if (piece === undefined) {
      break;
    }
    bytes += piece.byteLength;
    if (bytes > limit) {
      throw new Error(`${url} is larger than the ${limit} bytes left to load`);
    }
    pieces.push(piece.slice());
  }
  const gathered = new Uint8Array(bytes);
  let offset = 0;
  for (const piece of pieces) {
    gathered.set(piece, offset);
    offset += piece.byteLength;
  }
  return gathered;
}

// One of the modules a runtime with a file system comes with. A browser has
// none, and a bundler has nothing to resolve, since nothing is imported.
function builtin(name) {
  const found = globalThis.process?.getBuiltinModule?.(name);
  if (found === undefined) {
    throw new Error(`this runtime has no ${name}; pass a resolver to load documents`);
  }
  return found;
}

// The URL of the document asked for by `location`. Where files may be read,
// anything that is not a URL is the path of one.
export function locate(location, policy) {
  // A drive letter reads as a scheme of one letter.
  if (policy.files === "none" || /^[a-z][a-z0-9+.-]+:/i.test(location)) {
    return location;
  }
  return builtin("node:url").pathToFileURL(location).href;
}

const documentTypes = [".json", ".yaml", ".yml"];

// Reads at most `limit` bytes of the file at `url`. `root` is the URL of the
// document the caller asked for.
//
// It reads files on this machine only. Under the "folder" rule a document
// can make it read only in one folder or below it: the one the caller named,
// or the one that holds the file asked for. The folder is checked before the
// disk is touched and again once links are followed, so a link cannot lead
// out of it, and a file a document refers to has to be named as JSON or
// YAML, since any text reads as YAML and would be inlined. The "any" rule
// reads whatever file a document names.
async function loadFile(url, limit, root, policy) {
  const { fileURLToPath } = builtin("node:url");
  const { dirname, extname, isAbsolute, relative, resolve, sep } = builtin("node:path");
  const { open, realpath } = builtin("node:fs/promises");
  if (new URL(url).hostname !== "") {
    throw refused("read", url, "names another machine");
  }
  let path = fileURLToPath(url);
  if (policy.files === "folder") {
    if (policy.folder === undefined && new URL(root).protocol !== "file:") {
      throw new Error("load.folder has to name the folder files are read from");
    }
    const folder = policy.folder === undefined
      ? dirname(fileURLToPath(root))
      : /^file:/i.test(policy.folder)
      ? fileURLToPath(policy.folder)
      : resolve(policy.folder);
    const inside = (inner, holder) => {
      const from = relative(holder, inner);
      return from !== "" && from !== ".." && !from.startsWith(`..${sep}`) && !isAbsolute(from);
    };
    if (!inside(path, folder)) {
      throw refused("read", url, `is outside ${folder}`);
    }
    path = await realpath(path);
    if (!inside(path, await realpath(folder))) {
      throw refused("read", url, `is a link that leads outside ${folder}`);
    }
    if (url !== root && !documentTypes.includes(extname(path).toLowerCase())) {
      throw refused("read", url, `is not a ${documentTypes.join(", ")} file`);
    }
  }
  const handle = await open(path, "r");
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

// The addresses the host of `url` leads to, each one checked. A name anyone
// can register may lead to an address only this network can reach.
//
// A page makes its own connection, under the browser's rules, so there is
// nothing to check there. Anywhere else, a host that cannot be looked up
// cannot be held to an address, and is refused. A runtime that says it is a
// server is never taken for a page, whatever globals a page would have.
async function checked(url) {
  const dns = globalThis.process?.getBuiltinModule?.("node:dns/promises");
  if (dns === undefined) {
    const server = globalThis.process?.versions?.node !== undefined ||
      globalThis.Deno !== undefined ||
      globalThis.Bun !== undefined;
    if (!server && globalThis.window !== undefined && globalThis.document !== undefined) {
      return undefined;
    }
    throw refused("fetch", url, "leads somewhere this runtime cannot check");
  }
  const { BlockList } = builtin("node:net");
  const networks = new BlockList();
  for (const [network, prefix] of closedIPv4) {
    networks.addSubnet(network, prefix, "ipv4");
  }
  for (const [network, prefix] of closedIPv6) {
    networks.addSubnet(network, prefix, "ipv6");
  }
  const { hostname } = new URL(url);
  const found = await dns.lookup(hostname.replace(/^\[|\]$/g, "").replace(/\.$/, ""), {
    all: true,
    verbatim: true,
  });
  if (
    found.length === 0 ||
    found.some(({ address, family }) => networks.check(address, family === 6 ? "ipv6" : "ipv4"))
  ) {
    throw refused("fetch", url, "leads to a local or private address");
  }
  return found;
}

// The headers a connection to a checked address sets itself.
const fixed = [
  "accept-encoding",
  "connection",
  "content-length",
  "expect",
  "host",
  "keep-alive",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
];

const without = (headers, names) =>
  Object.fromEntries(
    Object.entries(headers).filter(([name]) => !names.includes(name.toLowerCase())),
  );

// Sends one request for `url`, which gets `policy.timeoutMs`. `own` says
// whether it may carry the headers and credentials the caller gave. With
// `pinned`, the addresses its host was checked to lead to, the connection is
// made to one of those, so that a second lookup cannot lead anywhere else.
async function get(url, pinned, limit, policy, own) {
  const signal = AbortSignal.timeout(policy.timeoutMs);
  const headers = own ? policy.headers ?? {} : {};
  if (pinned === undefined) {
    const response = await fetch(url, {
      redirect: "manual",
      signal,
      headers,
      credentials: own ? policy.credentials : "same-origin",
    });
    return {
      status: response.status,
      location: response.headers.get("location"),
      opaque: response.type === "opaqueredirect",
      bytes: async () => {
        if (response.body === null) {
          return new Uint8Array(0);
        }
        const reader = response.body.getReader();
        return gather(
          async () => {
            const { done, value } = await reader.read();
            return done ? undefined : value;
          },
          limit,
          url,
        );
      },
      discard: async () => {
        if (response.body !== null && !response.body.locked) {
          await response.body.cancel();
        }
      },
    };
  }
  const { request } = builtin(new URL(url).protocol === "https:" ? "node:https" : "node:http");
  const bare = (hostname) => hostname.replace(/^\[|\]$/g, "").replace(/\.$/, "").toLowerCase();
  const lookup = (hostname, options, callback) => {
    const family = options.family === "IPv4" ? 4 : options.family === "IPv6" ? 6 : options.family || 0;
    const fitting = bare(hostname) === bare(new URL(url).hostname)
      ? pinned.filter((address) => family === 0 || address.family === family)
      : [];
    if (fitting.length === 0) {
      callback(Object.assign(new Error(`no checked address for ${hostname}`), { code: "ENOTFOUND" }));
    } else if (options.all) {
      callback(null, fitting);
    } else {
      callback(null, fitting[0].address, fitting[0].family);
    }
  };
  const response = await new Promise((resolve, reject) => {
    const sent = request(
      url,
      {
        method: "GET",
        headers: { accept: "*/*", ...without(headers, fixed), "accept-encoding": "identity" },
        lookup,
        agent: false,
        signal,
      },
      resolve,
    );
    sent.on("error", reject);
    sent.end();
  });
  return {
    status: response.statusCode,
    location: response.headers.location ?? null,
    opaque: false,
    bytes: async () => {
      const encoding = response.headers["content-encoding"] ?? "identity";
      if (encoding !== "identity") {
        throw new Error(`${url} answered in the encoding ${encoding}, which was not asked for`);
      }
      const pieces = response[Symbol.asyncIterator]();
      return gather(
        async () => {
          const { done, value } = await pieces.next();
          return done ? undefined : value;
        },
        limit,
        url,
      );
    },
    discard: async () => {
      response.destroy();
    },
  };
}

// Loads at most `limit` bytes of a document the way a caller who gives no
// other way gets it, under the rules in `policy`. `root` is the URL of the
// document the caller asked for. A redirect is followed only where its
// target can be seen and passes the same checks. The headers and
// credentials the caller gave go only to the origin of `root`, since a
// document can name any origin and any of them may carry a secret.
async function loadBytes(url, limit, root, policy) {
  if (policy.files !== "none" && new URL(url).protocol === "file:") {
    return loadFile(url, limit, root, policy);
  }
  const origin = new URL(root).origin;
  let target = url;
  for (let hops = 0; hops <= policy.redirects; hops += 1) {
    const reason = refusal(target, root, policy);
    if (reason !== undefined) {
      throw refused("fetch", target, reason);
    }
    // A host the caller did not name is looked up and held to its answer.
    const pinned = policy.safeUrlResolver && !policy.sameOrigin
      ? await checked(target)
      : undefined;
    const response = await get(target, pinned, limit, policy, new URL(target).origin === origin);
    try {
      if (response.status >= 300 && response.status < 400 && response.location) {
        target = new URL(response.location, target).href;
        continue;
      }
      if (response.opaque) {
        throw new Error(
          `${target} redirects somewhere the default loader cannot check; pass a resolver to load it`,
        );
      }
      if (response.status < 200 || response.status >= 300) {
        throw new Error(`${target} answered ${response.status}`);
      }
      return await response.bytes();
    } finally {
      await response.discard();
    }
  }
  throw new Error(`${url} redirects more than ${policy.redirects} times`);
}

// `loadBytes`, read as UTF-8 text.
export async function load(url, limit, root, policy) {
  return new TextDecoder().decode(await loadBytes(url, limit, root, policy));
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn link(table: &str, into: &JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch)]
    fn load(url: &str, limit: usize, root: &str, policy: &JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch)]
    fn rewrite(into: &JsValue, result: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(catch)]
    fn locate(location: &str, policy: &JsValue) -> Result<String, JsValue>;
}

/// Which files the built-in loader of `dereferenceSchemaFrom` reads.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Tsify)]
#[serde(rename_all = "lowercase")]
pub enum Files {
    /// No file. A path or a `file:` URL is refused.
    #[default]
    None,
    /// `.json`, `.yaml` and `.yml` files in one folder and below it.
    Folder,
    /// Any file a document names, as json-schema-ref-parser reads them.
    Any,
}

/// Which requests of the built-in loader carry cookies and credentials, as
/// the `credentials` option of `fetch` says.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Tsify)]
#[serde(rename_all = "kebab-case")]
pub enum Credentials {
    /// Those to the origin of the page.
    #[default]
    SameOrigin,
    /// Every one, to any origin.
    Include,
    /// None.
    Omit,
}

/// What the built-in loader of `dereferenceSchemaFrom` may load.
///
/// A document names what it refers to, so the defaults bound what a document
/// can make the loader load. Turning `sameOrigin` off, setting `files` to
/// `"any"` and turning `text` on loads what json-schema-ref-parser loads.
#[derive(Clone, Debug, Deserialize, Serialize, Tsify)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct LoadOptions {
    /// Fetch only from the origin of the URL that was asked for. With this
    /// off, a runtime that can look a host up does so, refuses one that
    /// leads to a private address, and connects to the address it checked.
    pub same_origin: bool,
    /// Refuse a URL that names this machine or a private network.
    pub safe_url_resolver: bool,
    /// Which files may be read, where the runtime has a file system.
    pub files: Files,
    /// The folder [`Files::Folder`] reads from, as a path or a `file:` URL.
    /// Left out, it is the folder that holds the file that was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[tsify(optional)]
    pub folder: Option<String>,
    /// Take a document named as plain text, such as `.txt` or `.md`, for the
    /// string it holds, without reading it as JSON or YAML.
    pub text: bool,
    /// Headers to send with each request to the origin of the URL that was
    /// asked for. A request anywhere else, a redirect included, sends none.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[tsify(optional, type = "Record<string, string>")]
    pub headers: BTreeMap<String, String>,
    /// How long one request may take, in milliseconds.
    pub timeout_ms: u32,
    /// How many redirects one document may follow.
    pub redirects: u32,
    /// Which requests to the origin of the URL that was asked for carry
    /// cookies and credentials, in a browser. Requests anywhere else carry
    /// them only to the origin of the page.
    pub credentials: Credentials,
}

impl LoadOptions {
    /// The default for [`LoadOptions::timeout_ms`], a minute.
    pub const DEFAULT_TIMEOUT_MS: u32 = 60_000;
    /// The default for [`LoadOptions::redirects`].
    pub const DEFAULT_REDIRECTS: u32 = 5;
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            same_origin: true,
            safe_url_resolver: true,
            files: Files::None,
            folder: None,
            text: false,
            headers: BTreeMap::new(),
            timeout_ms: Self::DEFAULT_TIMEOUT_MS,
            redirects: Self::DEFAULT_REDIRECTS,
            credentials: Credentials::SameOrigin,
        }
    }
}

/// The members a JavaScript caller gives that only a JavaScript caller can.
#[wasm_bindgen(typescript_custom_section)]
const HOOKS: &str = r#"
/** Callbacks a run makes as it goes. */
export interface Hooks {
    /**
     * Whether to leave the value at `path` in the output, a JSON Pointer in a
     * URI fragment, and everything inside it as written. Asked of every
     * value the run reaches, and again of a target where a reference first
     * reaches it.
     */
    excluded?: (path: string, value: unknown) => boolean;
    /** Called with where each reference that takes part in a cycle is written. */
    onCircular?: (source: string) => void;
    /** Called with each reference replaced, what replaced it, and where. */
    onDereference?: (reference: string, value: unknown, parent: unknown, key: string | undefined) => void;
    /** Called with each member a bundle looked through that has a `$ref` member. */
    onBundle?: (reference: unknown, value: unknown, parent: unknown, key: string | undefined) => void;
}
"#;

/// The names of the members of `Hooks`.
const CALLBACKS: [&str; 4] = ["excluded", "onCircular", "onDereference", "onBundle"];

/// Options of a run that only a JavaScript caller sets.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Tsify)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct RunOptions {
    /// Stop with an error once the run has taken this many milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[tsify(optional)]
    pub timeout_ms: Option<u32>,
    /// Carry on past a reference that cannot be resolved, which becomes
    /// `null`, and list it in `errors`.
    pub continue_on_error: bool,
    /// Give every place that refers to a target its own copy of it, except
    /// where the copy would hold a cycle.
    pub clone_references: bool,
}

/// The names of the members of `RunOptions`.
const RUN_OPTIONS: [&str; 3] = ["timeoutMs", "continueOnError", "cloneReferences"];

/// A reference that could not be resolved, which a run carried on past.
#[derive(Clone, Debug, Serialize, Tsify)]
pub struct Unresolved {
    /// Why it could not be resolved.
    pub error: Error,
    /// Where the reference is written, as a URL with a JSON Pointer in its
    /// fragment.
    pub source: String,
}

/// A bundled document.
#[derive(Clone, Debug, Serialize, Tsify)]
pub struct Bundled {
    /// The bundle.
    #[tsify(type = "unknown")]
    pub schema: Value,
    /// The references a run with `continueOnError` carried on past.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[tsify(optional)]
    pub errors: Option<Vec<Unresolved>>,
}

/// The callbacks in `Hooks`.
#[derive(Default)]
struct Callbacks {
    excluded: Option<Function>,
    on_circular: Option<Function>,
    on_dereference: Option<Function>,
    on_bundle: Option<Function>,
}

/// What a JavaScript caller gave in its options.
struct Given {
    options: Options,
    run: RunOptions,
    callbacks: Callbacks,
    /// The member named for the function it was given to, if any.
    own: Option<Value>,
}

impl Given {
    /// Reads `options`, whose member `own` is for the function they were
    /// given to.
    fn read(options: &JsValue, own: &str) -> Result<Self, JsValue> {
        if options.is_undefined() || options.is_null() {
            return Ok(Self {
                options: Options::default(),
                run: RunOptions::default(),
                callbacks: Callbacks::default(),
                own: None,
            });
        }
        let callback = |name: &str| -> Result<Option<Function>, JsValue> {
            let member = Reflect::get(options, &JsValue::from_str(name))?;
            if member.is_undefined() {
                return Ok(None);
            }
            if !member.is_function() {
                return Err(JsError::new(&format!("{name} is not a function")).into());
            }
            Ok(Some(member.unchecked_into::<Function>()))
        };
        let callbacks = Callbacks {
            excluded: callback("excluded")?,
            on_circular: callback("onCircular")?,
            on_dereference: callback("onDereference")?,
            on_bundle: callback("onBundle")?,
        };
        let mut members: Map<String, Value> =
            serde_json::from_str(&json_of(options)?).map_err(thrown)?;
        for name in CALLBACKS {
            members.remove(name);
        }
        let own = members.remove(own);
        let run: Map<String, Value> = RUN_OPTIONS
            .iter()
            .filter_map(|name| {
                members
                    .remove(*name)
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect();
        Ok(Self {
            options: serde_json::from_value(Value::Object(members)).map_err(thrown)?,
            run: serde_json::from_value(Value::Object(run)).map_err(thrown)?,
            callbacks,
            own,
        })
    }

    /// The load options a function that loads documents was given.
    fn load(&self) -> Result<LoadOptions, JsValue> {
        match &self.own {
            Some(load) => serde_json::from_value(load.clone()).map_err(thrown),
            None => Ok(LoadOptions::default()),
        }
    }

    /// Whether the result is to be written into the caller's own object.
    fn mutates(&self) -> Result<bool, JsValue> {
        match &self.own {
            Some(Value::Bool(mutate)) => Ok(*mutate),
            None => Ok(false),
            Some(
                Value::Null
                | Value::Number(_)
                | Value::String(_)
                | Value::Array(_)
                | Value::Object(_),
            ) => Err(JsError::new("mutateInputSchema is not true or false").into()),
        }
    }
}

/// Where the documents a schema refers to come from.
enum Source {
    /// Whatever the caller's function returns for a URL.
    Resolver(Function),
    /// The built-in loader, under the rules the caller set.
    Loader {
        policy: JsValue,
        /// Whether a document named as plain text is the string it holds.
        text: bool,
    },
}

impl Source {
    fn of(
        url: String,
        load: &LoadOptions,
        resolve: Option<Function>,
    ) -> Result<(String, Self), JsValue> {
        if let Some(resolve) = resolve {
            return Ok((url, Self::Resolver(resolve)));
        }
        let policy = JSON::parse(&serde_json::to_string(load).map_err(thrown)?)?;
        let root = locate(&url, &policy)?;
        Ok((
            root,
            Self::Loader {
                policy,
                text: load.text,
            },
        ))
    }
}

/// Replaces every `$ref` in `schema` with the value it points at.
///
/// The places that refer to one target share one object, so changing it in
/// one place changes it in all of them. `schema` itself is left untouched,
/// unless `mutateInputSchema` is set: then the result is written into
/// `schema` and that same object is returned, as json-schema-ref-parser
/// does. The objects inside it are still new ones.
///
/// # Errors
///
/// Throws an `Error` when `schema` or `options` cannot be read, when a
/// reference cannot be resolved, unless `continueOnError` is set, when the
/// result exceeds a limit, and with whatever a callback throws.
#[wasm_bindgen(
    js_name = dereferenceSchema,
    unchecked_return_type = "Dereferenced & { errors?: Unresolved[] }"
)]
pub fn dereference_schema(
    schema: &JsValue,
    #[wasm_bindgen(
        unchecked_optional_param_type = "Options & RunOptions & Hooks & { mutateInputSchema?: boolean }"
    )]
    options: &JsValue,
) -> Result<JsValue, JsValue> {
    let given = Given::read(options, "mutateInputSchema")?;
    let into = if given.mutates()? {
        schema.clone()
    } else {
        JsValue::UNDEFINED
    };
    let document = serde_json::from_str(&json_of(schema)?).map_err(thrown)?;
    let root = Document {
        root: &document,
        url: None,
    };
    let mut hooks = JsHooks::new(&given, None, schema.clone());
    dereferenced(&root, None, &given, &into, &mut hooks)
}

/// Loads the document at `url` and every document its references lead to,
/// then replaces every `$ref` in it with the value it points at, as
/// `dereferenceSchema` does.
///
/// `resolve` is called with the absolute URL of each document and returns
/// the document or a promise of it. References in a document are relative to
/// its own URL.
///
/// Without `resolve`, each document is loaded and read as JSON, or as YAML
/// in a build that has it, under rules that bound what a document can make
/// it load: only `http:` and `https:` URLs, never one that names this
/// machine or a private network, and only from the origin of `url`.
/// Redirects are held to the same rules. The `load` member of `options`
/// changes the rules, and lets `url` be the path of a file.
///
/// # Errors
///
/// Rejects when a document cannot be loaded or read, when the documents
/// together are larger than `maxBytes` or more than `maxDocuments`, and for
/// every reason `dereferenceSchema` throws.
#[wasm_bindgen(
    js_name = dereferenceSchemaFrom,
    unchecked_return_type = "Promise<Dereferenced & { errors?: Unresolved[] }>"
)]
pub async fn dereference_schema_from(
    url: String,
    #[wasm_bindgen(
        unchecked_optional_param_type = "Options & RunOptions & Hooks & { load?: LoadOptions }"
    )]
    options: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "(url: string) => unknown")] resolve: Option<
        Function,
    >,
) -> Result<JsValue, JsValue> {
    let given = Given::read(&options, "load")?;
    let (root, source) = Source::of(url, &given.load()?, resolve)?;
    let documents = gathered(&root, &given.options, &source).await?;
    let root = documents.root(&root).map_err(|error| failure(&error))?;
    let mut hooks = JsHooks::new(&given, Some(&documents), JsValue::UNDEFINED);
    dereferenced(
        &root,
        Some(&documents),
        &given,
        &JsValue::UNDEFINED,
        &mut hooks,
    )
}

/// Bundles `schema`: every reference in it is pointed straight at the value
/// it leads to, past any reference on the way, unless `internal` is
/// `"written"`. `schema` is left untouched, unless `mutateInputSchema` is
/// set: then the bundle is written into `schema`.
///
/// # Errors
///
/// Throws when `schema` or `options` cannot be read, when a reference cannot
/// be resolved or points outside `schema`, unless `continueOnError` is set,
/// and with whatever a callback throws.
#[wasm_bindgen(js_name = bundleSchema)]
pub fn bundle_schema(
    schema: &JsValue,
    #[wasm_bindgen(
        unchecked_optional_param_type = "Options & RunOptions & Hooks & { mutateInputSchema?: boolean }"
    )]
    options: &JsValue,
) -> Result<Ts<Bundled>, JsValue> {
    let given = Given::read(options, "mutateInputSchema")?;
    let document: Value = serde_json::from_str(&json_of(schema)?).map_err(thrown)?;
    let mut hooks = JsHooks::new(&given, None, schema.clone());
    let bundled = crate::bundle_schema_with_hooks(&document, &given.options, &mut hooks);
    let bundled = hooks.finish(bundled)?;
    let into = if given.mutates()? {
        schema.clone()
    } else {
        JsValue::UNDEFINED
    };
    hooks.after_bundle(bundled, &into)
}

/// Loads the document at `url` and every document its references lead to, as
/// `dereferenceSchemaFrom` does, and bundles them: what references to other
/// documents point at is written into the document, once each, at the first
/// place that refers to it as json-schema-ref-parser chooses it, and every
/// other reference to it, or into it, points there. Every reference left
/// points within the bundle.
///
/// # Errors
///
/// Rejects for the reasons `dereferenceSchemaFrom` does, and when the bundle
/// nests deeper than `maxDepth`.
#[wasm_bindgen(js_name = bundleSchemaFrom, unchecked_return_type = "Promise<Bundled>")]
pub async fn bundle_schema_from(
    url: String,
    #[wasm_bindgen(
        unchecked_optional_param_type = "Options & RunOptions & Hooks & { load?: LoadOptions }"
    )]
    options: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "(url: string) => unknown")] resolve: Option<
        Function,
    >,
) -> Result<JsValue, JsValue> {
    let given = Given::read(&options, "load")?;
    let (root, source) = Source::of(url, &given.load()?, resolve)?;
    let documents = gathered(&root, &given.options, &source).await?;
    bundled(&documents, &root, &given).map(|bundled| bundled.js_value().clone())
}

/// Loads the document at `url` and every document its references lead to,
/// as `dereferenceSchemaFrom` does, without dereferencing them. With
/// `external` set to `"ignore"`, only the document at `url` is loaded.
///
/// # Errors
///
/// Rejects when a document cannot be loaded or read, and when the documents
/// together are larger than `maxBytes` or more than `maxDocuments`.
#[wasm_bindgen(js_name = resolveSchemaFrom)]
pub async fn resolve_schema_from(
    url: String,
    #[wasm_bindgen(unchecked_optional_param_type = "Options & { load?: LoadOptions }")]
    options: JsValue,
    #[wasm_bindgen(unchecked_optional_param_type = "(url: string) => unknown")] resolve: Option<
        Function,
    >,
) -> Result<Resolved, JsValue> {
    let given = Given::read(&options, "load")?;
    let (root, source) = Source::of(url, &given.load()?, resolve)?;
    let documents = gathered(&root, &given.options, &source).await?;
    let url = Documents::normalized(&root).map_err(|error| failure(&error))?;
    Ok(Resolved { documents, url })
}

/// A document and every document its references lead to, loaded.
#[wasm_bindgen]
#[derive(Debug)]
pub struct Resolved {
    documents: Documents,
    url: String,
}

#[wasm_bindgen]
impl Resolved {
    /// The absolute URL of the document that was asked for.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The absolute URLs of every document loaded, sorted.
    #[must_use]
    pub fn urls(&self) -> Vec<String> {
        self.documents
            .urls()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    }

    /// The value `reference` points at, a copy. `reference` is resolved
    /// against the URL of the document that was asked for, so `#/a` is in
    /// that document. A pointer passes through any reference on its way, and
    /// a bare reference where it ends is followed.
    ///
    /// # Errors
    ///
    /// Throws when the document is not loaded or the pointer does not
    /// resolve.
    pub fn get(&self, reference: &str) -> Result<JsValue, JsValue> {
        let value = self
            .documents
            .lookup(&self.absolute(reference)?, &Options::default())
            .map_err(|error| failure(&error))?;
        JSON::parse(&serde_json::to_string(&value).map_err(thrown)?)
    }

    /// Whether `reference` points at a value, as `get` resolves it.
    #[must_use]
    pub fn exists(&self, reference: &str) -> bool {
        match self.absolute(reference) {
            Ok(reference) => self
                .documents
                .lookup(&reference, &Options::default())
                .is_ok(),
            Err(_) => false,
        }
    }

    /// Puts `value` where `reference` points, resolved as `get` resolves it.
    /// Members missing on the way are added as empty objects, and `-` as the
    /// last token of a pointer into an array adds an item at its end.
    ///
    /// # Errors
    ///
    /// Throws when the document is not loaded, when the pointer passes
    /// through a value that holds nothing, and when `value` is not JSON
    /// data.
    pub fn set(&mut self, reference: &str, value: &JsValue) -> Result<(), JsValue> {
        let value = serde_json::from_str(&json_of(value)?).map_err(thrown)?;
        let reference = self.absolute(reference)?;
        self.documents
            .set(&reference, value)
            .map_err(|error| failure(&error))
    }

    /// Every document loaded, by absolute URL.
    ///
    /// # Errors
    ///
    /// Throws only if a document cannot be handed over.
    #[wasm_bindgen(js_name = toJSON, unchecked_return_type = "Record<string, unknown>")]
    pub fn to_json(&self) -> Result<JsValue, JsValue> {
        let documents: Map<String, Value> = self
            .documents
            .urls()
            .into_iter()
            .filter_map(|url| {
                self.documents
                    .document(url)
                    .map(|document| (url.to_owned(), document.clone()))
            })
            .collect();
        JSON::parse(&serde_json::to_string(&documents).map_err(thrown)?)
    }

    /// Dereferences the document that was asked for, as
    /// `dereferenceSchemaFrom` does.
    ///
    /// # Errors
    ///
    /// Throws for the reasons `dereferenceSchema` does.
    #[wasm_bindgen(unchecked_return_type = "Dereferenced & { errors?: Unresolved[] }")]
    pub fn dereference(
        &self,
        #[wasm_bindgen(unchecked_optional_param_type = "Options & RunOptions & Hooks")]
        options: &JsValue,
    ) -> Result<JsValue, JsValue> {
        let given = Given::read(options, "")?;
        let root = self
            .documents
            .root(&self.url)
            .map_err(|error| failure(&error))?;
        let mut hooks = JsHooks::new(&given, Some(&self.documents), JsValue::UNDEFINED);
        dereferenced(
            &root,
            Some(&self.documents),
            &given,
            &JsValue::UNDEFINED,
            &mut hooks,
        )
    }

    /// Bundles the document that was asked for, as `bundleSchemaFrom` does.
    ///
    /// # Errors
    ///
    /// Throws for the reasons `bundleSchema` does.
    pub fn bundle(
        &self,
        #[wasm_bindgen(unchecked_optional_param_type = "Options & RunOptions & Hooks")]
        options: &JsValue,
    ) -> Result<Ts<Bundled>, JsValue> {
        let given = Given::read(options, "")?;
        bundled(&self.documents, &self.url, &given)
    }

    /// `reference` resolved against the URL of the document asked for.
    fn absolute(&self, reference: &str) -> Result<String, JsValue> {
        let (location, fragment) = crate::documents::split(reference);
        let document = if location.is_empty() {
            self.url.clone()
        } else {
            crate::documents::absolute(Some(&self.url), location)
                .map_err(|error| failure(&error))?
        };
        Ok(format!("{document}#{fragment}"))
    }
}

/// Dereferences `root` with what the caller gave, and tells the callbacks
/// what happened.
fn dereferenced(
    root: &Document<'_>,
    documents: Option<&Documents>,
    given: &Given,
    into: &JsValue,
    hooks: &mut JsHooks<'_>,
) -> Result<JsValue, JsValue> {
    let copies = if given.run.clone_references {
        Copies::Own
    } else {
        Copies::Shared
    };
    let linked = linked(root, documents, &given.options, into, hooks, copies)?;
    hooks.after_dereference(&linked)?;
    Ok(linked)
}

/// Bundles the document at `url` among `documents` with what the caller
/// gave.
fn bundled(documents: &Documents, url: &str, given: &Given) -> Result<Ts<Bundled>, JsValue> {
    let mut hooks = JsHooks::new(given, Some(documents), JsValue::UNDEFINED);
    let bundled = documents.bundle_with_hooks(url, &given.options, &mut hooks);
    let bundled = hooks.finish(bundled)?;
    hooks.after_bundle(bundled, &JsValue::UNDEFINED)
}

/// Loads the document at `url` and every document it leads to from
/// `source`.
async fn gathered(url: &str, options: &Options, source: &Source) -> Result<Documents, JsValue> {
    let mut documents = Documents::new();
    let mut loaded: usize = 0;
    // What the documents may still take between them.
    let mut bytes_left = options.max_bytes;
    let root = Documents::normalized(url).map_err(|error| failure(&error))?;
    let mut asked = BTreeSet::from([root.clone()]);
    let mut wanted = vec![root.clone()];
    while let Some(next) = wanted.pop() {
        if loaded >= options.max_documents {
            return Err(failure(&Error::DocumentLimitExceeded {
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
            // The loader stops reading at what is left, so a document too
            // large is never held in full.
            Source::Loader { policy, .. } => {
                JsFuture::from(Promise::resolve(&load(&next, bytes_left, &root, policy)?))
                    .await?
                    .as_string()
                    .ok_or_else(|| JsError::new("the loader did not return text"))?
            }
        };
        bytes_left = bytes_left.checked_sub(text.len()).ok_or_else(|| {
            failure(&Error::SizeLimitExceeded {
                limit: options.max_bytes,
            })
        })?;
        let document = match source {
            Source::Resolver(_) => serde_json::from_str(&text).map_err(thrown)?,
            Source::Loader { text: true, .. } if is_text(&next) => Value::String(text),
            Source::Loader { .. } => read(&text, options)?,
        };
        documents
            .insert(&next, document)
            .map_err(|error| failure(&error))?;
        // Only what this document refers to is new, so only that is looked
        // at, however many documents came before.
        for referred in documents
            .referred(&next, options)
            .map_err(|error| failure(&error))?
        {
            if asked.insert(referred.clone()) {
                wanted.push(referred);
            }
        }
    }
    Ok(documents)
}

/// The names that say a document is plain text.
const TEXT_TYPES: [&str; 12] = [
    "txt", "htm", "html", "md", "xml", "js", "min", "map", "css", "scss", "less", "svg",
];

/// Whether the document at `url` is plain text by its name, and so is the
/// string it holds, as json-schema-ref-parser has it.
fn is_text(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    path.rsplit_once('.').is_some_and(|(_, kind)| {
        TEXT_TYPES
            .iter()
            .any(|text| kind.eq_ignore_ascii_case(text))
    })
}

/// Whether every place that refers to a target holds the same object.
#[derive(Clone, Copy)]
enum Copies {
    /// They share it.
    Shared,
    /// Each has its own copy, except where a copy would hold a cycle.
    Own,
}

/// Dereferences `root` and hands the result to JavaScript as shared objects.
/// `into` is the caller's own object to write the result into, if any.
fn linked(
    root: &Document<'_>,
    documents: Option<&Documents>,
    options: &Options,
    into: &JsValue,
    hooks: &mut JsHooks<'_>,
    copies: Copies,
) -> Result<JsValue, JsValue> {
    // The limit is on what the result takes, and shared objects take no more
    // room for being referred to often. The table counts what it writes.
    let expanded = expand(root, documents, options, Form::Linked, hooks);
    let expanded = hooks.finish(expanded)?;
    let table = Table::write(&expanded.schema, options, copies)?;
    // The links in the schema have been followed, so what they point at can
    // go.
    drop(expanded.anchors);
    link(&table, into)
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
    let shared = dereference_schema_shared(&schema, &options).map_err(|error| failure(&error))?;
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

/// A step of a path, as JavaScript reads a member.
fn key_of(step: Step<'_>) -> JsValue {
    match step {
        Step::Name(name) => JsValue::from_str(name),
        Step::Index(position) => JsValue::from_str(&position.to_string()),
    }
}

/// The value at the end of `path` in `root`, the value holding it, and its
/// name there.
fn walk(root: &JsValue, path: &[JsValue]) -> Result<(JsValue, JsValue, JsValue), JsValue> {
    let mut parent = JsValue::UNDEFINED;
    let mut key = JsValue::UNDEFINED;
    let mut value = root.clone();
    for step in path {
        if !value.is_object() {
            return Ok((JsValue::UNDEFINED, parent, key));
        }
        let next = Reflect::get(&value, step)?;
        parent = value;
        key = step.clone();
        value = next;
    }
    Ok((value, parent, key))
}

/// What the hooks of a JavaScript run do.
struct JsHooks<'run> {
    callbacks: &'run Callbacks,
    /// The documents of the run, if it has more than the one given.
    documents: Option<&'run Documents>,
    /// The caller's own document, where it gave one with no URL.
    root: JsValue,
    /// The documents handed to JavaScript so far, by URL.
    converted: BTreeMap<String, JsValue>,
    /// When the run has to be done by, as `Date.now` counts, and the limit
    /// that came from.
    deadline: Option<(f64, u64)>,
    continue_on_error: bool,
    /// What a callback threw, which ends the run.
    thrown: Option<JsValue>,
    replaced: Vec<(String, Vec<JsValue>)>,
    circular: Vec<String>,
    /// Where each member a bundle looked through is written.
    looked_through: Vec<(Option<String>, Vec<JsValue>)>,
    errors: Vec<Unresolved>,
}

impl<'run> JsHooks<'run> {
    fn new(given: &'run Given, documents: Option<&'run Documents>, root: JsValue) -> Self {
        Self {
            callbacks: &given.callbacks,
            documents,
            root,
            converted: BTreeMap::new(),
            deadline: given
                .run
                .timeout_ms
                .map(|limit| (Date::now() + f64::from(limit), u64::from(limit))),
            continue_on_error: given.run.continue_on_error,
            thrown: None,
            replaced: Vec::new(),
            circular: Vec::new(),
            looked_through: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// The outcome of a run. What a callback threw takes the place of the
    /// error that stopped the run because of it.
    fn finish<T>(&mut self, outcome: Result<T, Error>) -> Result<T, JsValue> {
        if let Some(thrown) = self.thrown.take() {
            return Err(thrown);
        }
        outcome.map_err(|error| failure(&error))
    }

    /// The document at `url`, or the caller's own, as JavaScript holds it.
    fn document(&mut self, url: Option<&str>) -> Result<JsValue, JsValue> {
        let Some(url) = url else {
            return Ok(self.root.clone());
        };
        if let Some(converted) = self.converted.get(url) {
            return Ok(converted.clone());
        }
        let converted = match self.documents.and_then(|documents| documents.document(url)) {
            Some(document) => JSON::parse(&serde_json::to_string(document).map_err(thrown)?)?,
            None => JsValue::UNDEFINED,
        };
        self.converted.insert(url.to_owned(), converted.clone());
        Ok(converted)
    }

    /// The JavaScript value written where `place` is.
    fn value_of(&mut self, place: &Place<'_, '_>) -> Result<JsValue, JsValue> {
        let source = place.source();
        let document = self.document(source.url)?;
        let path: Vec<JsValue> = source.path.into_iter().map(key_of).collect();
        Ok(walk(&document, &path)?.0)
    }

    /// Hands the errors a run carried on past to the caller, and tells the
    /// callbacks what a dereference did.
    fn after_dereference(&mut self, linked: &JsValue) -> Result<(), JsValue> {
        self.errors_onto(linked)?;
        let schema = Reflect::get(linked, &JsValue::from_str("schema"))?;
        if let Some(callback) = &self.callbacks.on_dereference {
            for (reference, path) in &self.replaced {
                let (value, parent, key) = walk(&schema, path)?;
                let arguments =
                    js_sys::Array::of4(&JsValue::from_str(reference), &value, &parent, &key);
                callback.apply(&JsValue::NULL, &arguments)?;
            }
        }
        if let Some(callback) = &self.callbacks.on_circular {
            for source in &self.circular {
                callback.call1(&JsValue::NULL, &JsValue::from_str(source))?;
            }
        }
        Ok(())
    }

    /// Hands a bundle to the caller, written into `into` if that is an
    /// object, and tells the callbacks what it looked through.
    fn after_bundle(&mut self, schema: Value, into: &JsValue) -> Result<Ts<Bundled>, JsValue> {
        if let Some(callback) = self.callbacks.on_bundle.clone() {
            for (url, path) in core::mem::take(&mut self.looked_through) {
                let document = self.document(url.as_deref())?;
                let (value, parent, key) = walk(&document, &path)?;
                let reference = if value.is_object() {
                    Reflect::get(&value, &JsValue::from_str("$ref"))?
                } else {
                    JsValue::UNDEFINED
                };
                let arguments = js_sys::Array::of4(&reference, &value, &parent, &key);
                callback.apply(&JsValue::NULL, &arguments)?;
            }
        }
        let errors = self
            .continue_on_error
            .then(|| core::mem::take(&mut self.errors));
        let text = serde_json::to_string(&Bundled { schema, errors }).map_err(thrown)?;
        let bundled = JSON::parse(&text)?;
        if into.is_object() {
            let schema = Reflect::get(&bundled, &JsValue::from_str("schema"))?;
            rewrite(into, &schema)?;
            Reflect::set(&bundled, &JsValue::from_str("schema"), into)?;
        }
        Ok(Ts::new_unchecked(bundled))
    }

    /// Sets the `errors` member of `result` where the run carried on past
    /// errors.
    fn errors_onto(&mut self, result: &JsValue) -> Result<(), JsValue> {
        if self.continue_on_error {
            let errors = serde_json::to_string(&self.errors).map_err(thrown)?;
            Reflect::set(result, &JsValue::from_str("errors"), &JSON::parse(&errors)?)?;
        }
        Ok(())
    }
}

impl Hooks for JsHooks<'_> {
    fn excluded(&mut self, place: &Place<'_, '_>) -> Result<bool, Error> {
        let Some(excluded) = self.callbacks.excluded.clone() else {
            return Ok(false);
        };
        let answer = self.value_of(place).and_then(|value| {
            excluded.call2(
                &JsValue::NULL,
                &JsValue::from_str(&fragment(place.path())),
                &value,
            )
        });
        match answer {
            Ok(answer) => Ok(answer.is_truthy()),
            Err(thrown) => {
                self.thrown = Some(thrown);
                Err(Error::Interrupted)
            }
        }
    }

    fn circular(&mut self, place: &Place<'_, '_>) {
        if self.callbacks.on_circular.is_some() {
            self.circular.push(place.source().to_string());
        }
    }

    fn dereferenced(&mut self, place: &Place<'_, '_>) {
        if self.callbacks.on_dereference.is_some()
            && let Some(reference) = reference_of(place.value())
        {
            let path = place.path().iter().copied().map(key_of).collect();
            self.replaced.push((reference.to_owned(), path));
        }
    }

    fn bundled(&mut self, place: &Place<'_, '_>) {
        if self.callbacks.on_bundle.is_some() {
            let source = place.source();
            let path = source.path.into_iter().map(key_of).collect();
            self.looked_through
                .push((source.url.map(ToOwned::to_owned), path));
        }
    }

    fn failed(&mut self, place: &Place<'_, '_>, error: Error) -> Result<(), Error> {
        if !self.continue_on_error {
            return Err(error);
        }
        self.errors.push(Unresolved {
            error,
            source: place.source().to_string(),
        });
        Ok(())
    }

    fn proceed(&mut self) -> Result<(), Error> {
        match self.deadline {
            Some((deadline, limit)) if Date::now() > deadline => {
                Err(Error::TimeLimitExceeded { limit })
            }
            Some(_) | None => Ok(()),
        }
    }

    fn observes(&self) -> bool {
        self.callbacks.excluded.is_some()
            || self.callbacks.on_circular.is_some()
            || self.callbacks.on_dereference.is_some()
            || self.callbacks.on_bundle.is_some()
    }
}

/// Reads a document the default loader fetched, which is JSON, or YAML in a
/// build that has it.
fn read(text: &str, options: &Options) -> Result<Value, JsValue> {
    let json = match serde_json::from_str(text) {
        Ok(document) => return Ok(document),
        Err(error) => error,
    };
    match yaml(text, options) {
        Ok(document) => Ok(document),
        // Text that opens like JSON was meant as JSON, so that reader's
        // complaint is the one to pass on, as it is in a build without YAML.
        Err(_) if cfg!(not(feature = "yaml")) || text.trim_start().starts_with(['{', '[']) => {
            Err(thrown(json))
        }
        Err(error) => Err(error),
    }
}

/// Reads YAML, which this build cannot.
#[cfg(not(feature = "yaml"))]
fn yaml(_: &str, _: &Options) -> Result<Value, JsValue> {
    Err(JsError::new("this build of unbinder reads no YAML").into())
}

/// Reads YAML.
#[cfg(feature = "yaml")]
fn yaml(text: &str, options: &Options) -> Result<Value, JsValue> {
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
    serde_saphyr::from_str_with_options(text, limits).map_err(thrown)
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

/// `error` as a thrown `Error` whose `detail` says which error it is.
fn failure(error: &Error) -> JsValue {
    let failure: JsValue = JsError::new(&error.to_string()).into();
    let detail = match serde_json::to_string(error) {
        Ok(detail) => JSON::parse(&detail),
        Err(problem) => return thrown(problem),
    };
    match detail.and_then(|detail| Reflect::set(&failure, &JsValue::from_str("detail"), &detail)) {
        Ok(_) => failure,
        Err(problem) => problem,
    }
}

/// Why the table could not be written.
enum Failure {
    /// The result takes more than a limit allows.
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
            Failure::Limit(error) => crate::wasm::failure(&error),
            Failure::Json(error) => thrown(error),
        }
    }
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
    copies: Copies,
    /// The targets a cycle can be reached from, which are shared even where
    /// each place gets its own copy.
    cyclic: BTreeSet<*const Anchor<'doc>>,
}

impl<'doc, 'options> Table<'doc, 'options> {
    fn write(
        schema: &SharedSchema<'doc>,
        options: &'options Options,
        copies: Copies,
    ) -> Result<String, Failure> {
        let mut table = Self {
            options,
            bytes_left: options.max_bytes,
            entries: BTreeMap::new(),
            count: 0,
            targets: BTreeMap::new(),
            pending: Vec::new(),
            copies,
            cyclic: match copies {
                Copies::Shared => BTreeSet::new(),
                Copies::Own => cyclic(schema.shared()),
            },
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

    /// Whether `target` is written out at each place that holds it.
    fn copied(&self, target: &Rc<Anchor<'doc>>) -> bool {
        match self.copies {
            Copies::Shared => false,
            Copies::Own => !self.cyclic.contains(&Rc::as_ptr(target)),
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
            copying: 0,
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

/// The targets in `root` that lead to a cycle: one they take part in, or one
/// further in.
fn cyclic<'doc>(root: &Shared<'doc>) -> BTreeSet<*const Anchor<'doc>> {
    /// A target being looked through, with the targets it holds.
    struct Frame<'doc> {
        target: Rc<Anchor<'doc>>,
        held: Vec<Rc<Anchor<'doc>>>,
        next: usize,
        cyclic: bool,
    }
    enum State {
        Open,
        Closed { cyclic: bool },
    }
    let mut states: BTreeMap<*const Anchor<'doc>, State> = BTreeMap::new();
    for start in held(root) {
        if states.contains_key(&Rc::as_ptr(&start)) {
            continue;
        }
        states.insert(Rc::as_ptr(&start), State::Open);
        let mut frames = vec![Frame {
            held: held(&start.value),
            target: start,
            next: 0,
            cyclic: false,
        }];
        while let Some(frame) = frames.last_mut() {
            let Some(next) = frame.held.get(frame.next).map(Rc::clone) else {
                let Some(done) = frames.pop() else {
                    break;
                };
                states.insert(
                    Rc::as_ptr(&done.target),
                    State::Closed {
                        cyclic: done.cyclic,
                    },
                );
                if let Some(parent) = frames.last_mut() {
                    parent.cyclic |= done.cyclic;
                }
                continue;
            };
            frame.next = frame.next.saturating_add(1);
            match states.get(&Rc::as_ptr(&next)) {
                Some(State::Open) => frame.cyclic = true,
                Some(State::Closed { cyclic }) => frame.cyclic |= *cyclic,
                None => {
                    states.insert(Rc::as_ptr(&next), State::Open);
                    frames.push(Frame {
                        held: held(&next.value),
                        target: next,
                        next: 0,
                        cyclic: false,
                    });
                }
            }
        }
    }
    states
        .into_iter()
        .filter_map(|(target, state)| match state {
            State::Closed { cyclic: true } => Some(target),
            State::Closed { cyclic: false } | State::Open => None,
        })
        .collect()
}

/// The targets `value` holds, without looking inside them.
fn held<'doc>(value: &Shared<'doc>) -> Vec<Rc<Anchor<'doc>>> {
    let mut found = Vec::new();
    let mut pending = vec![value.clone()];
    while let Some(current) = pending.pop() {
        match current {
            Shared::Original(_) => {}
            Shared::Array(items) => pending.extend(items.iter().cloned()),
            Shared::Object(members) => {
                pending.extend(members.iter().map(|(_, member)| member.clone()));
            }
            Shared::Inlined(target) => found.push(target),
            Shared::Merged(merged) => {
                pending.push(merged.base.clone());
                pending.extend(merged.overrides.iter().map(|(_, member)| member.clone()));
            }
            Shared::Back(back) => found.extend(back.target()),
        }
    }
    found
}

/// One entry of the table as it is written.
struct Entry<'table, 'doc, 'options> {
    table: &'table mut Table<'doc, 'options>,
    /// The value, with `null` wherever a link fills it in.
    plain: Vec<u8>,
    /// The path to the value being written.
    path: Vec<Step<'doc>>,
    links: Vec<Link<'doc>>,
    /// How many targets are being written out in place, one inside another.
    copying: usize,
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

    /// Writes `target` out in place of linking to it.
    fn copy(&mut self, target: &Shared<'doc>) -> Result<(), Failure> {
        if self.copying >= self.table.options.max_depth {
            return Err(Failure::Limit(Error::DepthLimitExceeded {
                limit: self.table.options.max_depth,
            }));
        }
        self.copying = self.copying.saturating_add(1);
        let written = self.value(target);
        self.copying = self.copying.saturating_sub(1);
        written
    }

    /// Writes the place being written as `target`, linked or copied.
    fn take(&mut self, target: &Rc<Anchor<'doc>>) -> Result<(), Failure> {
        if self.table.copied(target) {
            return self.copy(&target.value);
        }
        self.link(target, false)?;
        self.plain.extend(b"null");
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
                    self.path.push(Step::Index(position));
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
            Shared::Inlined(target) => self.take(target),
            Shared::Back(back) => match back.target() {
                Some(target) => self.take(&target),
                None => self.write(back.written),
            },
            Shared::Merged(merged) => {
                self.plain.push(b'{');
                let mut first = true;
                match &merged.base {
                    Shared::Inlined(target) if !self.table.copied(target) => {
                        // The object takes the target's members from the one
                        // shared object, so only its own are written here.
                        self.link(target, true)?;
                        for (name, member) in &merged.overrides {
                            self.member(&mut first, name, member)?;
                        }
                    }
                    Shared::Original(_)
                    | Shared::Array(_)
                    | Shared::Object(_)
                    | Shared::Inlined(_)
                    | Shared::Merged(_)
                    | Shared::Back(_) => {
                        value.members(&mut |name, member| self.member(&mut first, name, member))?;
                    }
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
