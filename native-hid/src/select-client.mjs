// Picks which interface and driver class of a brand to talk to.
//
// A receiver can expose several interfaces that a driver class opens and
// reads without error, but only one of them carries the mouse's battery: the
// Keychron TurboLink 8K answers Keychron8kHidClient on its consumer, vendor
// 0xffc1 and 0x8c interfaces, and only 0xffc1 reports the G6 HE 8K's battery.
// Taking the first answer would report no battery at all, so an answer with a
// battery wins, and the first answer is only used when none has one (a wired
// mouse, for instance).
//
// Once a class has answered, only that class is tried on the remaining
// interfaces: the class is known to fit the device, and trying every other
// class on every interface could take long enough (each failed probe may
// wait out its timeout) to run past Bridge's limit on the helper.
//
// A battery-less answer is closed before probing on rather than held open:
// some receivers stop answering on one collection while a sibling is read
// (see candidateDevices()), so it is probed again if it ends up the pick.

/**
 * @param {Array<{ device: object, candidate: object }>} pairs - every
 *   (interface, driver class) combination, in preference order.
 * @param {(device: object, candidate: object) => Promise<{ client: object, status: object }>} probe
 *   opens and reads one pair; rejects (and leaves the device closed) on failure.
 * @returns {Promise<{ client: object | null, status: object | null, attempts: string[] }>}
 */
export async function selectClient(pairs, probe) {
  const attempts = [];
  let fallback = null;
  for (const { device, candidate } of pairs) {
    if (fallback && (candidate.exportName !== fallback.candidate.exportName || device === fallback.device)) continue;
    let found;
    try {
      found = await probe(device, candidate);
    } catch (error) {
      attempts.push(`${candidate.exportName} on ${device.productName || "unknown device"}: ${error.message}`);
      continue;
    }
    if (hasBattery(found.status)) return { ...found, attempts };
    fallback ??= { device, candidate };
    await found.client.close?.().catch(() => undefined);
    await device.close?.().catch(() => undefined);
  }
  if (fallback) {
    try {
      return { ...(await probe(fallback.device, fallback.candidate)), attempts };
    } catch (error) {
      attempts.push(`${fallback.candidate.exportName} on ${fallback.device.productName || "unknown device"}: ${error.message}`);
    }
  }
  return { client: null, status: null, attempts };
}

function hasBattery(status) {
  return typeof status?.batteryPercent === "number" && Number.isFinite(status.batteryPercent);
}
