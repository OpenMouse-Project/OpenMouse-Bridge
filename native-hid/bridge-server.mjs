import http from 'node:http';
import { WebSocketServer } from 'ws';
import { candidateDevices } from './src/hid-device-adapter.mjs';
import { BRAND_DRIVERS, brandKey } from './src/brands.mjs';

// Polyfill `window` for mouse-protocol drivers that use window.setTimeout.
globalThis.window ??= globalThis;

const PROBE_TIMEOUT_MS = 3000;

function withTimeout(promise, ms, label) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Try one candidate driver class against a HidDeviceAdapter. */
async function probe(device, candidate) {
  const module = await import(candidate.module);
  const Client = module[candidate.exportName];
  const client = new Client(device);
  try {
    await withTimeout(client.open(), PROBE_TIMEOUT_MS, `${candidate.exportName}.open()`);
    await withTimeout(client.readStatus(), PROBE_TIMEOUT_MS, `${candidate.exportName}.readStatus()`);
    return client;
  } catch (err) {
    await client.close().catch(() => {});
    throw err;
  }
}

/** Open the best-matching driver for a brand and return { client, status }. */
async function probeForBrand(brand) {
  const entry = BRAND_DRIVERS[brandKey(brand)];
  if (!entry) return null;
  for (const vendorId of entry.vendorIds) {
    for (const device of candidateDevices(vendorId)) {
      for (const candidate of entry.classes) {
        try {
          const client = await probe(device, candidate);
          const status = await withTimeout(client.readStatus(), PROBE_TIMEOUT_MS, 'readStatus');
          return { client, status };
        } catch {
          // try next candidate
        }
      }
    }
  }
  return null;
}

const PORT = 17846;

const server = http.createServer((req, res) => {
  res.setHeader('Access-Control-Allow-Origin', '*');
  res.setHeader('Access-Control-Allow-Methods', 'GET, PUT, POST, OPTIONS');
  res.setHeader('Access-Control-Allow-Headers', 'Content-Type');

  if (req.method === 'OPTIONS') {
    res.writeHead(200);
    res.end();
    return;
  }

  if (req.url === '/v1/status') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({
      version: "1.0.0",
      platform: "macos",
      linuxDistribution: null,
      uptimeSeconds: Math.floor(process.uptime()),
      activeGames: [],
      trackedGameCount: 0,
      batteryThresholdPercent: 20,
      autostartEnabled: false,
      foregroundApplication: null,
      activeProfile: null,
      visibleApplicationCount: 0,
      profileCount: 0,
      clientConnected: true
    }));
    return;
  }

  if (req.url === '/v1/handshake') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ ok: true }));
    return;
  }

  res.writeHead(404);
  res.end();
});

const wss = new WebSocketServer({ noServer: true });

server.on('upgrade', (request, socket, head) => {
  if (request.url === '/v1/hid') {
    wss.handleUpgrade(request, socket, head, (ws) => {
      wss.emit('connection', ws, request);
    });
  } else {
    socket.destroy();
  }
});

// Battery signature used by X11-family (AttackShark / Delux M800 Mini)
// wireless receivers: report ID 0x03 + bytes [0x55, 0x40, 0x01, <pct>].
const BATTERY_REPORT_ID = 0x03;
const BATTERY_SIGNATURE = [0x55, 0x40, 0x01];

class ManagedDevice {
  constructor(key, candidate) {
    this.key = key;
    this.vendorId = candidate.vendorId;
    this.productId = candidate.productId;
    this.productName = (candidate.vendorId === 0x1d57 && (candidate.productId === 0xfa60 || candidate.productId === 0xfa55))
      ? "Delux M800 Mini"
      : (candidate.productName || "Delux Mouse");
    this.path = candidate._infos[0]?.path;
    this.adapter = candidate;
    this._listeners = new Set();
    /** Latest battery percent seen on this device's input stream, or null. */
    this.batteryPercent = null;
    this.batteryAt = 0;
    this._batteryMonitor = null;
  }

  /** Start listening for autonomous battery packets from the receiver. */
  startBatteryMonitor() {
    if (this._batteryMonitor) return;
    const handler = (evt) => {
      if (evt.reportId !== BATTERY_REPORT_ID) return;
      const data = new Uint8Array(evt.data.buffer, evt.data.byteOffset, evt.data.byteLength);
      if (data.length < 4) return;
      if (data[0] !== BATTERY_SIGNATURE[0] || data[1] !== BATTERY_SIGNATURE[1] || data[2] !== BATTERY_SIGNATURE[2]) return;
      const pct = data[3];
      this.batteryPercent = pct;
      this.batteryAt = Date.now();
      console.log(`[Bridge] 🔋 Battery update for ${this.productName}: ${pct}%`);
      // Push battery event to all connected WebSocket clients.
      const event = JSON.stringify({
        type: 'battery',
        device: this.key,
        batteryPercent: pct,
        batteryState: 'Discharging',
      });
      for (const ws of activeSockets) {
        try { ws.send(event); } catch { /* client gone */ }
      }
    };
    this._batteryMonitor = handler;
    this.adapter.addEventListener('inputreport', handler);
  }

  stopBatteryMonitor() {
    if (!this._batteryMonitor) return;
    this.adapter.removeEventListener('inputreport', this._batteryMonitor);
    this._batteryMonitor = null;
  }

  updateIfPathChanged(candidate) {
    const newPath = candidate._infos[0]?.path;
    if (this.path !== newPath) {
      console.log(`[Bridge] Device ${this.key} path changed: ${this.path} -> ${newPath}`);
      if (this.adapter.opened) {
        this.adapter.close().catch(() => {});
      }
      this.path = newPath;
      this.adapter = candidate;
    }
  }

  async ensureOpen() {
    if (this.adapter.opened) return;
    try {
      await this.adapter.open();
    } catch (err) {
      console.warn(`[Bridge] Failed to open ${this.path} (${err.message}), trying to rediscover...`);
      const candidates = candidateDevices(this.vendorId);
      const match = candidates.find((c) => c.productId === this.productId);
      if (match) {
        this.updateIfPathChanged(match);
        await this.adapter.open();
        console.log(`[Bridge] Successfully re-opened on new path: ${this.path}`);
      } else {
        throw err;
      }
    }
  }

  async close() {
    if (this.adapter.opened) {
      await this.adapter.close().catch(() => {});
    }
  }

  async sendFeatureReport(reportId, data) {
    await this.ensureOpen();
    try {
      await this.adapter.sendFeatureReport(reportId, data);
    } catch (err) {
      console.warn(`[Bridge] sendFeatureReport failed (${err.message}), retrying with fresh open...`);
      await this.close();
      await this.ensureOpen();
      await this.adapter.sendFeatureReport(reportId, data);
    }
  }

  async receiveFeatureReport(reportId) {
    await this.ensureOpen();
    try {
      return await this.adapter.receiveFeatureReport(reportId);
    } catch (err) {
      console.warn(`[Bridge] receiveFeatureReport failed (${err.message}), retrying with fresh open...`);
      await this.close();
      await this.ensureOpen();
      return await this.adapter.receiveFeatureReport(reportId);
    }
  }
}

const openHandles = new Map();
const activeSockets = new Set();
let cleanupTimer = null;

wss.on('connection', (ws) => {
  activeSockets.add(ws);
  if (cleanupTimer) {
    clearTimeout(cleanupTimer);
    cleanupTimer = null;
  }
  console.log(`[Bridge] WebUI connected via WebSocket (active clients: ${activeSockets.size})`);

  ws.on('message', async (raw) => {
    let msg;
    try {
      msg = JSON.parse(raw.toString());
    } catch {
      return;
    }

    const { id, type } = msg;

    if (type === 'list') {
      const vendorIds = msg.vendorIds || [0x1d57, 0x248a];
      const found = [];
      const currentKeys = new Set();

      for (const vid of vendorIds) {
        const candidates = candidateDevices(vid);
        for (let i = 0; i < candidates.length; i++) {
          const c = candidates[i];
          const key = `${c.vendorId.toString(16)}:${c.productId.toString(16)}:${i}`;
          currentKeys.add(key);

          let dev = openHandles.get(key);
          if (!dev) {
            dev = new ManagedDevice(key, c);
            openHandles.set(key, dev);
          } else {
            dev.updateIfPathChanged(c);
          }

          found.push({
            key,
            vendorId: dev.vendorId,
            productId: dev.productId,
            productName: dev.productName,
            collections: []
          });
        }
      }

      // Remove handles that no longer exist on the USB bus
      for (const [key, dev] of openHandles.entries()) {
        if (!currentKeys.has(key)) {
          dev.close().catch(() => {});
          openHandles.delete(key);
        }
      }

      ws.send(JSON.stringify({ id, ok: true, devices: found }));
      return;
    }

    if (type === 'open') {
      const dev = openHandles.get(msg.device);
      if (dev) {
        try {
          await dev.ensureOpen();
          console.log(`[Bridge] Device opened: ${dev.productName} (${dev.path})`);
          // Start passive battery monitor for wireless X11-family devices.
          dev.startBatteryMonitor();
          ws.send(JSON.stringify({ id, ok: true }));
        } catch (err) {
          console.error(`[Bridge] Failed to open device: ${err.message}`);
          ws.send(JSON.stringify({ id, ok: false, error: err.message }));
        }
      } else {
        ws.send(JSON.stringify({ id, ok: false, error: 'Device not found' }));
      }
      return;
    }

    if (type === 'close') {
      const dev = openHandles.get(msg.device);
      if (dev) {
        await dev.close();
      }
      ws.send(JSON.stringify({ id, ok: true }));
      return;
    }

    if (type === 'sendFeatureReport') {
      const dev = openHandles.get(msg.device);
      if (dev) {
        try {
          const data = new Uint8Array(msg.data);
          await dev.sendFeatureReport(msg.reportId, data);
          ws.send(JSON.stringify({ id, ok: true }));
        } catch (err) {
          console.error(`[Bridge] sendFeatureReport error: ${err.message}`);
          ws.send(JSON.stringify({ id, ok: false, error: err.message }));
        }
      } else {
        ws.send(JSON.stringify({ id, ok: false, error: 'Device not found' }));
      }
      return;
    }

    if (type === 'receiveFeatureReport') {
      const dev = openHandles.get(msg.device);
      if (dev) {
        try {
          const res = await dev.receiveFeatureReport(msg.reportId);
          const bytes = Array.from(new Uint8Array(res.buffer, res.byteOffset, res.byteLength));
          ws.send(JSON.stringify({ id, ok: true, data: bytes }));
        } catch (err) {
          console.error(`[Bridge] receiveFeatureReport error: ${err.message}`);
          ws.send(JSON.stringify({ id, ok: false, error: err.message }));
        }
      } else {
        ws.send(JSON.stringify({ id, ok: false, error: 'Device not found' }));
      }
      return;
    }

    if (type === 'readStatus') {
      // Probe the correct driver for the given brand and return a MouseStatus
      // snapshot (DPI, polling rate, batteryPercent, etc.).
      const brand = msg.brand ?? 'delux';
      const dev = openHandles.get(msg.device);
      try {
        const result = await probeForBrand(brand);
        if (!result) {
          ws.send(JSON.stringify({ id, ok: false, error: `No ${brand} device answered` }));
          return;
        }
        const { client, status } = result;
        // If we already saw a battery % from the passive monitor, prefer it
        // (it's more recent than what readStatus() may have waited for).
        if (dev && dev.batteryPercent !== null && dev.batteryAt > Date.now() - 60_000) {
          status.batteryPercent = dev.batteryPercent;
          status.batteryState = 'Discharging';
        }
        await client.close().catch(() => {});
        ws.send(JSON.stringify({ id, ok: true, status }));
      } catch (err) {
        ws.send(JSON.stringify({ id, ok: false, error: err.message }));
      }
      return;
    }

    if (type === 'listen') {
      const dev = openHandles.get(msg.device);
      if (dev) {
        await dev.ensureOpen();
        // Also start the battery monitor alongside the raw report stream.
        dev.startBatteryMonitor();
        const handler = (evt) => {
          const bytes = Array.from(new Uint8Array(evt.data.buffer, evt.data.byteOffset, evt.data.byteLength));
          ws.send(JSON.stringify({
            type: 'report',
            device: msg.device,
            reportId: evt.reportId,
            data: bytes
          }));
        };
        dev._bridgeListener = handler;
        dev.adapter.addEventListener('inputreport', handler);
        ws.send(JSON.stringify({ id, ok: true }));
      }
      return;
    }

    if (type === 'unlisten') {
      const dev = openHandles.get(msg.device);
      if (dev && dev._bridgeListener) {
        dev.adapter.removeEventListener('inputreport', dev._bridgeListener);
        delete dev._bridgeListener;
      }
      // Do NOT stop the battery monitor — keep it running so the passive
      // push updates continue for all connected clients.
      ws.send(JSON.stringify({ id, ok: true }));
      return;
    }
  });

  ws.on('close', () => {
    activeSockets.delete(ws);
    console.log(`[Bridge] WebUI client disconnected (active clients: ${activeSockets.size})`);
    if (activeSockets.size === 0) {
      // Set grace period before closing devices in case of quick page reload
      cleanupTimer = setTimeout(async () => {
        if (activeSockets.size === 0) {
          console.log('[Bridge] All clients disconnected, closing device handles...');
          for (const dev of openHandles.values()) {
            dev.stopBatteryMonitor();
            await dev.close().catch(() => {});
          }
        }
      }, 10000);
    }
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[Bridge] OpenMouse Bridge native server listening on http://127.0.0.1:${PORT}`);
});
