// Puts the documents the benchmark runs on into the fixtures directory: two
// synthetic ones generated from a fixed seed, and two real ones downloaded at
// a pinned revision and checked against a pinned digest.

import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { fixtures } from "./paths.js";

const downloads = [
  {
    name: "kubernetes",
    url:
      "https://raw.githubusercontent.com/kubernetes/kubernetes/v1.37.1/api/openapi-spec/swagger.json",
    sha256: "465276aedf437726de5a6ed23a41b86d78bb0d347edf251a755ae26a93cdd8a6",
  },
  {
    name: "stripe",
    url:
      "https://raw.githubusercontent.com/stripe/openapi/v2561/openapi/spec3.json",
    sha256: "7cff4cc46d0654101a36a3302f7544640f5f73e4f3cad6f264f0e23c19fa1776",
  },
];

function digest(content) {
  return createHash("sha256").update(content).digest("hex");
}

/** A linear congruential generator, so the documents do not vary. */
function sequence(seed) {
  let state = seed;
  return (bound) => {
    state = (state * 1_103_515_245 + 12_345) % 2_147_483_648;
    return state % bound;
  };
}

/**
 * A schema the size a form or a config file has: a few shared definitions,
 * one of them recursive.
 */
function small() {
  const fields = ["street", "city", "region", "postcode", "country"];
  return {
    $schema: "http://json-schema.org/draft-07/schema#",
    type: "object",
    required: ["id", "name"],
    properties: {
      id: { $ref: "#/definitions/id" },
      name: { $ref: "#/definitions/text", description: "Display name" },
      email: { $ref: "#/definitions/text", format: "email" },
      address: { $ref: "#/definitions/address" },
      billing: { $ref: "#/definitions/address" },
      tags: { type: "array", items: { $ref: "#/definitions/text" } },
      team: { $ref: "#/definitions/team" },
    },
    definitions: {
      id: { type: "string", pattern: "^[a-z0-9]{12}$" },
      text: { type: "string", minLength: 1, maxLength: 255 },
      address: {
        type: "object",
        properties: Object.fromEntries(
          fields.map((field) => [field, { $ref: "#/definitions/text" }]),
        ),
      },
      team: {
        type: "object",
        properties: {
          id: { $ref: "#/definitions/id" },
          parent: { $ref: "#/definitions/team" },
        },
      },
    },
  };
}

/**
 * Five layers of eighty definitions. Each definition refers to three in the
 * layer below, and the document uses two hundred from the top layer.
 */
function layered() {
  const next = sequence(20_261_013);
  const layers = 5;
  const width = 80;
  const definitions = {};
  for (let layer = 0; layer < layers; layer += 1) {
    for (let index = 0; index < width; index += 1) {
      const properties = {};
      for (let slot = 0; slot < 3; slot += 1) {
        properties[`field${slot}`] = layer + 1 < layers
          ? { $ref: `#/definitions/layer${layer + 1}item${next(width)}` }
          : { type: "string", description: `Leaf ${index} ${slot}` };
      }
      definitions[`layer${layer}item${index}`] = {
        type: "object",
        description: `Definition ${index} of layer ${layer}`,
        properties,
      };
    }
  }
  const properties = {};
  for (let index = 0; index < 200; index += 1) {
    properties[`use${index}`] = {
      $ref: `#/definitions/layer0item${next(width)}`,
    };
  }
  return { type: "object", properties, definitions };
}

mkdirSync(fixtures, { recursive: true });
for (const [name, document] of [["small", small()], ["layered", layered()]]) {
  writeFileSync(join(fixtures, `${name}.json`), JSON.stringify(document));
}
for (const { name, url, sha256 } of downloads) {
  const path = join(fixtures, `${name}.json`);
  if (existsSync(path) && digest(readFileSync(path)) === sha256) {
    continue;
  }
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url} answered ${response.status}`);
  }
  const content = Buffer.from(await response.arrayBuffer());
  if (digest(content) !== sha256) {
    throw new Error(`${url} does not match its pinned digest`);
  }
  writeFileSync(path, content);
}
