import assert from "node:assert/strict";
import test from "node:test";

import { GWOLVES_PRODUCTS } from "@openmouse/protocol/drivers/gwolves/products";

import { BRAND_DRIVERS, classesFor } from "./brands.mjs";

const names = (classes) => classes.map((candidate) => candidate.exportName);

test("each G-Wolves product is probed with the class of its own protocol only", () => {
  const gwolves = BRAND_DRIVERS["g-wolves"];
  assert.deepEqual(names(classesFor(gwolves, 0x5317)), ["GWolvesXviHidClient"]);
  assert.deepEqual(names(classesFor(gwolves, 0x3817)), ["GWolvesXviNewHidClient"]);
  assert.deepEqual(names(classesFor(gwolves, 0x5618)), ["GWolvesHidClient"]);
  for (const productId of GWOLVES_PRODUCTS.keys()) {
    assert.equal(classesFor(gwolves, productId).length, 1, `product 0x${productId.toString(16)}`);
  }
  assert.deepEqual(classesFor(gwolves, 0xffff), []);
});

test("brands without product ids keep probing every class", () => {
  for (const [brand, entry] of Object.entries(BRAND_DRIVERS)) {
    if (entry.classes.some((candidate) => candidate.productIds)) continue;
    assert.deepEqual(classesFor(entry, 0x1234), entry.classes, brand);
  }
});
