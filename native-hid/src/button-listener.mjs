#!/usr/bin/env node
// Long-running helper Bridge spawns while a game profile has button
// assignments for a Logitech mouse running from software (Host mode).
//
// stdin: one JSON line to start, e.g. {"brand": "Logitech", "device": "PRO X 3 SUPERSTRIKE"}
//   ("device" picks that mouse when several of the brand are connected); add
//   "requireHost": true to listen only to a mouse already on Host. Closing stdin (or
//   SIGTERM/SIGINT) stops it: the listener is switched off and the mouse goes back
//   to the mode it was in, if this helper changed it.
// stdout: one JSON object per line.
//   {"event": "ready", "priorMode": "Onboard", "buttons": [1, 2, 3, 4, 5]}
//   {"event": "buttons", "mask": 16}   — the buttons held right now
//   {"event": "error", "message": "..."}
//   {"event": "stopped"}
//
// Exit codes: 0 stopped cleanly, 3 no driver for the brand, 1 anything else.
// See apply.mjs for why `window` is aliased before any driver is imported.
globalThis.window ??= globalThis;

import { createInterface } from "node:readline";

import { BRAND_DRIVERS, brandKey } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

const EXIT_OK = 0;
const EXIT_FAILED = 1;
const EXIT_NO_DRIVER = 3;
const STEP_TIMEOUT_MS = 3000;

const emit = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);

function withTimeout(promise, ms, label) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Opens a candidate class on one device and reads its status, or rejects and closes it. */
async function probe(device, candidate) {
  const module = await import(candidate.module);
  const client = new module[candidate.exportName](device);
  try {
    await withTimeout(client.open(), STEP_TIMEOUT_MS, `${candidate.exportName}.open()`);
    const status = await withTimeout(client.readStatus(), STEP_TIMEOUT_MS, `${candidate.exportName}.readStatus()`);
    return { client, status };
  } catch (error) {
    await device.close().catch(() => undefined);
    throw error;
  }
}

// `wantedName` is the mouse the profile names. With several mice of the brand
// connected only that one is used; with a single one it is used whatever its
// name, so a name spelled differently never leaves it without a listener.
async function findClient(entry, wantedName) {
  const attempts = [];
  const others = [];
  const closeOthers = () => Promise.all(others.map((other) => other.client.close().catch(() => undefined)));
  for (const vendorId of entry.vendorIds) {
    for (const device of candidateDevices(vendorId)) {
      for (const candidate of entry.classes) {
        try {
          const found = await probe(device, candidate);
          // The listener only exists on a client built from a protocol release that has it.
          if (typeof found.client.startButtonListener !== "function") {
            await found.client.close().catch(() => undefined);
            attempts.push(`${candidate.exportName}: this protocol version has no button listener`);
            continue;
          }
          if (!wantedName || String(found.status?.name ?? "").toLowerCase() === wantedName.toLowerCase()) {
            await closeOthers();
            return found;
          }
          others.push(found);
          break;
        } catch (error) {
          attempts.push(`${candidate.exportName} on ${device.productName || "unknown device"}: ${error.message}`);
        }
      }
    }
  }
  if (others.length === 1) return others[0];
  await closeOthers();
  if (others.length > 1) throw new Error(`none of the ${others.length} connected mice is named "${wantedName}"`);
  throw new Error(`no mouse answered. ${attempts.join("; ")}`);
}

async function main(input) {
  const entry = BRAND_DRIVERS[brandKey(typeof input.brand === "string" ? input.brand : "")];
  if (!entry) {
    emit({ event: "error", message: `no driver registered for brand "${input.brand}"` });
    process.exit(EXIT_NO_DRIVER);
  }

  // Listening from the start: Bridge may close stdin while discovery is still
  // probing, and an `end` emitted with no listener is not emitted again.
  let stopRequested = false;
  let stop = () => {
    stopRequested = true;
  };
  const onStop = () => stop();
  process.stdin.on("end", onStop);
  process.stdin.on("close", onStop);
  process.on("SIGTERM", onStop);
  process.on("SIGINT", onStop);

  const { client, status } = await findClient(entry, typeof input.device === "string" ? input.device.trim() : "");
  if (stopRequested || process.stdin.readableEnded) {
    // Nothing has been changed on the mouse yet.
    await client.close().catch(() => undefined);
    emit({ event: "stopped" });
    process.exit(EXIT_OK);
  }
  const priorMode = status?.deviceMode ?? "Unknown";
  // The default profile only listens to a mouse already on Host; it never
  // switches one over, so a mouse on an onboard profile is left alone.
  if (input.requireHost === true && priorMode !== "Host") {
    emit({ event: "error", message: "the mouse is not on the Host profile" });
    await client.close().catch(() => undefined);
    process.exit(EXIT_FAILED);
  }
  let changedMode = false;
  let stopping = false;

  // `code` is how the helper ends: a failed start is a failure, not a clean stop.
  async function shutdown(code = EXIT_OK) {
    if (stopping) return;
    stopping = true;
    await withTimeout(client.stopButtonListener(), STEP_TIMEOUT_MS, "stopButtonListener").catch(() => undefined);
    if (changedMode) {
      await withTimeout(client.setOnboardMode(priorMode), STEP_TIMEOUT_MS, "setOnboardMode").catch(() => undefined);
    }
    await client.close().catch(() => undefined);
    emit({ event: "stopped" });
    process.exit(code);
  }

  // Event handlers pass their own arguments (a signal name), which are not an exit code.
  stop = () => void shutdown();

  try {
    // The events only flow while the mouse runs from software; an unknown mode
    // is left alone and the listener is tried as it is.
    if (priorMode === "Onboard") {
      // Marked first: the mouse may switch and then fail to confirm it, and it still has to go back.
      changedMode = true;
      await withTimeout(client.setOnboardMode("Host"), STEP_TIMEOUT_MS, "setOnboardMode");
    }
    client.onButtonChange((mask) => emit({ event: "buttons", mask }));
    const buttons = await withTimeout(client.startButtonListener(), STEP_TIMEOUT_MS, "startButtonListener");
    // A stop that came in while switching modes: put the mouse back now.
    if (stopRequested || process.stdin.readableEnded) {
      await shutdown();
      return;
    }
    emit({ event: "ready", priorMode, buttons });
  } catch (error) {
    emit({ event: "error", message: error.message });
    await shutdown(EXIT_FAILED);
  }
}

const lines = createInterface({ input: process.stdin });
lines.once("line", (line) => {
  let input;
  try {
    input = JSON.parse(line);
  } catch {
    emit({ event: "error", message: "the first line on stdin must be JSON" });
    process.exit(EXIT_FAILED);
  }
  main(input).catch((error) => {
    emit({ event: "error", message: error.message });
    process.exit(EXIT_FAILED);
  });
});
