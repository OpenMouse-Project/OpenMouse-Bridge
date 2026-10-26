const identityFields = ["vendorId", "productId", "path", "serialNumber", "interface"];

export async function automate(input, io) {
  if (input.brand?.trim().toLowerCase() !== "zaunkoenig") throw new Error("unsupported acknowledged native automation readback for this driver");
  const infos = io.list();
  if (infos.length > 1024) throw new Error("native identity enumeration limit exceeded");
  const endpoints = new Map();
  for (const info of infos) {
    if (info.vendorId !== 0x0483 || (info.usagePage === 1 && [1, 2, 6].includes(info.usage))) continue;
    const identity = Object.fromEntries(identityFields.map((field) => [field, info[field] ?? null]));
    endpoints.set(JSON.stringify(identity), { info, identity });
  }
  let selected;
  if (input.identity) {
    selected = [...endpoints.values()].find(({ identity }) => identityFields.every((field) => identity[field] === input.identity[field]));
  } else if (endpoints.size === 1) {
    selected = endpoints.values().next().value;
  }
  if (!selected || !Number.isInteger(selected.identity.interface) || !Number.isInteger(selected.identity.productId) || typeof selected.identity.path !== "string" || !selected.identity.path || selected.identity.path.length > 2048 || typeof selected.identity.serialNumber !== "string" || !selected.identity.serialNumber || selected.identity.serialNumber.length > 256) throw new Error("native device identity is missing, ambiguous, or changed");
  if (input.action === "automatic-apply" && !input.identity) throw new Error("native apply requires captured identity");
  const { client, status } = await io.probe(selected.info, selected.identity);
  try {
    if (`${status.brand}:${status.name}` !== input.deviceId) throw new Error("native profile device ownership does not match acknowledged status");
    if (!Number.isSafeInteger(status.dpi) || status.dpi <= 0 || !Number.isSafeInteger(status.pollingRateHz) || status.pollingRateHz <= 0 || (status.dpiY !== undefined && status.dpiY !== status.dpi)) throw new Error("unsupported acknowledged DPI/polling readback");
    if (input.action === "automatic-read") return { identity: selected.identity, dpi: status.dpi, pollingRateHz: status.pollingRateHz };
    if (input.action !== "automatic-apply") throw new Error("unsupported automatic action");
    for (const value of [input.dpi, input.pollingRateHz]) {
      if (value !== undefined && (!Number.isSafeInteger(value) || value <= 0)) throw new Error("invalid automatic settings");
    }
    if (input.pollingRateHz !== undefined && (!["Full", "High"].includes(status.usbSpeed) || ![1000, 2000, 4000, 8000].includes(input.pollingRateHz) || (input.pollingRateHz > 1000 && status.usbSpeed !== "High"))) throw new Error("automatic polling cannot change USB speed or use unsupported rates");
    if (input.dpi !== undefined && await client.setDpi(input.dpi) !== input.dpi) throw new Error("driver did not acknowledge requested DPI");
    if (input.pollingRateHz !== undefined && input.pollingRateHz !== status.pollingRateHz && await client.setPollingRate(input.pollingRateHz) !== input.pollingRateHz) throw new Error("driver did not acknowledge requested polling rate");
    return { identity: selected.identity };
  } finally {
    await client.close();
  }
}
