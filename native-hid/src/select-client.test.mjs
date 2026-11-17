import { test } from "node:test";
import assert from "node:assert/strict";

import { selectClient } from "./select-client.mjs";
import { BRAND_DRIVERS } from "./brands.mjs";

function fakeDevice(name) {
  return { productName: name, closed: 0, close: async function () { this.closed += 1; } };
}

const candidate = (exportName) => ({ exportName });

/** A probe that answers from `replies`, keyed by device name. */
function probeFrom(replies, log = []) {
  return async (device, which) => {
    log.push(device.productName);
    const reply = replies[device.productName];
    if (reply instanceof Error) throw reply;
    return { client: { close: async () => undefined, which }, status: reply };
  };
}

test("an interface with a battery wins over an earlier one without", async () => {
  const consumer = fakeDevice("consumer");
  const vendor = fakeDevice("vendor");
  const result = await selectClient(
    [
      { device: consumer, candidate: candidate("A") },
      { device: vendor, candidate: candidate("A") },
    ],
    probeFrom({ consumer: { batteryPercent: null }, vendor: { batteryPercent: 22 } }),
  );
  assert.equal(result.status.batteryPercent, 22);
  // The battery-less interface was let go before the next one was read.
  assert.equal(consumer.closed, 1);
});

test("the first answer is used when no interface reports a battery", async () => {
  const log = [];
  const result = await selectClient(
    [
      { device: fakeDevice("first"), candidate: candidate("A") },
      { device: fakeDevice("second"), candidate: candidate("A") },
    ],
    probeFrom({ first: { batteryPercent: null, dpi: 800 }, second: { batteryPercent: null, dpi: 1600 } }, log),
  );
  assert.equal(result.status.dpi, 800);
  // Probed again after the others, since it was closed in between.
  assert.deepEqual(log, ["first", "second", "first"]);
});

test("after a battery-less answer only that class is tried on other interfaces", async () => {
  const tried = [];
  const first = fakeDevice("first");
  const second = fakeDevice("second");
  const [a, b] = [candidate("A"), candidate("B")];
  const result = await selectClient(
    [
      { device: first, candidate: a },
      { device: first, candidate: b },
      { device: second, candidate: b },
      { device: second, candidate: a },
    ],
    async (device, which) => {
      tried.push(`${which.exportName}@${device.productName}`);
      return { client: { close: async () => undefined }, status: { batteryPercent: device === second ? 40 : null } };
    },
  );
  assert.equal(result.status.batteryPercent, 40);
  assert.deepEqual(tried, ["A@first", "A@second"]);
});

test("a battery answer stops the search", async () => {
  const log = [];
  await selectClient(
    [
      { device: fakeDevice("first"), candidate: candidate("A") },
      { device: fakeDevice("second"), candidate: candidate("A") },
    ],
    probeFrom({ first: { batteryPercent: 50 }, second: { batteryPercent: 60 } }, log),
  );
  assert.deepEqual(log, ["first"]);
});

test("failures are reported when nothing answers", async () => {
  const result = await selectClient(
    [{ device: fakeDevice("only"), candidate: candidate("A") }],
    probeFrom({ only: new Error("no reply") }),
  );
  assert.equal(result.client, null);
  assert.deepEqual(result.attempts, ["A on only: no reply"]);
});

test("every driver class a brand names exists in the protocol package", async () => {
  // Some driver modules touch `window` when loaded, as apply.mjs allows for.
  globalThis.window ??= globalThis;
  for (const [brand, entry] of Object.entries(BRAND_DRIVERS)) {
    for (const { module, exportName } of entry.classes) {
      const loaded = await import(module);
      assert.equal(typeof loaded[exportName], "function", `${brand}: ${module} has no ${exportName}`);
    }
  }
});
