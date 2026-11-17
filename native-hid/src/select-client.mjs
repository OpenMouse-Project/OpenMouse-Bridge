// Picks which interface and driver class of a brand to talk to.
//
// Settings writes take the first pair that answers, so a request always
// changes the same mouse it always did.
//
// A status read prefers an answer with a battery. A receiver can expose
// several interfaces that a driver class opens and reads without error, but
// only one of them carries the mouse's battery: the Keychron TurboLink 8K
// answers Keychron8kHidClient on its consumer, vendor 0xffc1 and 0x8c
// interfaces, and only 0xffc1 reports the G6 HE 8K's battery. Taking the first
// answer would report no battery at all, so an answer with a battery wins, and
// the first answer's status is used when none has one (a wired mouse, for
// instance).
//
// Once a class has answered, only that class is tried on the remaining
// interfaces: the class is known to fit the device, and trying every other
// class on every interface could take long enough (each failed probe may
// wait out its timeout) to run past Bridge's limit on the helper. That
// search also has a budget of its own, so stalled sibling interfaces cannot
// hold up a read whose status is already in hand.
//
// A battery-less answer is closed before probing on rather than held open:
// some receivers stop answering on one collection while a sibling is read
// (see candidateDevices()). Its status is kept, so it is never read twice.

/** How long a status read keeps looking for a battery after a first answer
 * without one. Bridge stops the helper after 10 s. */
const BATTERY_SEARCH_MS = 4000;

/**
 * @param {Array<{ device: object, candidate: object }>} pairs - every
 *   (interface, driver class) combination, in preference order.
 * @param {(device: object, candidate: object) => Promise<{ client: object, status: object }>} probe
 *   opens and reads one pair; rejects (and leaves the device closed) on failure.
 * @param {{ preferBattery?: boolean, batterySearchMs?: number }} [options] -
 *   `preferBattery` for status reads only: the returned client may then be
 *   closed already, and only its status is to be used.
 * @returns {Promise<{ client: object | null, status: object | null, attempts: string[] }>}
 */
export async function selectClient(pairs, probe, { preferBattery = false, batterySearchMs = BATTERY_SEARCH_MS } = {}) {
  const attempts = [];
  let fallback = null;
  let deadline = Infinity;
  for (const { device, candidate } of pairs) {
    if (fallback && (candidate.exportName !== fallback.candidate.exportName || device === fallback.device)) continue;
    const remaining = deadline - Date.now();
    if (remaining <= 0) break;
    let found;
    try {
      found = await (fallback ? within(probe(device, candidate), remaining) : probe(device, candidate));
    } catch (error) {
      attempts.push(`${candidate.exportName} on ${device.productName || "unknown device"}: ${error.message}`);
      continue;
    }
    if (!preferBattery || hasBattery(found.status)) return { ...found, attempts };
    if (!fallback) {
      fallback = { device, candidate, ...found };
      deadline = Date.now() + batterySearchMs;
    }
    await found.client.close?.().catch(() => undefined);
    await device.close?.().catch(() => undefined);
  }
  if (fallback) return { client: fallback.client, status: fallback.status, attempts };
  return { client: null, status: null, attempts };
}

function within(promise, ms) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(`no answer within the ${ms} ms left for a battery search`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

function hasBattery(status) {
  return typeof status?.batteryPercent === "number" && Number.isFinite(status.batteryPercent);
}
