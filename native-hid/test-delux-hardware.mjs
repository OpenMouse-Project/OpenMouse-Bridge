// Comprehensive hardware verification suite for Delux M800 Mini on macOS
globalThis.window ??= globalThis;

import { candidateDevices } from "./src/hid-device-adapter.mjs";
import { BRAND_DRIVERS } from "./src/brands.mjs";

const DELAY_MS = 250;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function runTestSuite() {
  console.log("============================================================");
  console.log("       DELUX M800 MINI HARDWARE VERIFICATION SUITE         ");
  console.log("============================================================\n");

  // TEST 1: Enumeration & Detection
  console.log("👉 TEST 1: Enumerating candidates for Vendor ID 0x1D57...");
  const devs = candidateDevices(0x1d57);
  console.log(`   Found ${devs.length} logical HID candidate interface(s).`);
  if (devs.length === 0) {
    throw new Error("FAIL: No 0x1d57 device found on system.");
  }
  console.log("   ✅ PASSED: Device detected on USB bus.\n");

  // TEST 2: Open and Probe Driver Class
  console.log("👉 TEST 2: Probing candidate driver classes...");
  const { AttackSharkHidClient } = await import("@openmouse/protocol/drivers/attackshark/hid");
  let activeClient = null;
  let activeDevice = null;

  for (const d of devs) {
    try {
      const client = new AttackSharkHidClient(d);
      await client.open();
      await client.setPollingRate(1000);
      const status = await client.readStatus();
      activeClient = client;
      activeDevice = d;
      console.log("   ✅ Successfully claimed interface:");
      console.log(`      - Product: ${d.productName}`);
      console.log(`      - PID: 0x${d.productId.toString(16)}`);
      console.log(`      - Initial DPI: ${status.dpi}`);
      console.log(`      - Initial Polling Rate: ${status.pollingRateHz} Hz`);
      console.log(`      - Supported Rates: [${status.supportedPollingRates.join(", ")}] Hz`);
      break;
    } catch (e) {
      // expected for interfaces that are not interface 2 or require root
    }
  }

  if (!activeClient) {
    throw new Error("FAIL: Could not open any valid HID interface for Delux M800 Mini.");
  }
  console.log("   ✅ PASSED: Driver class successfully probed and opened.\n");

  // TEST 3: Polling Rate Verification Cycle
  console.log("👉 TEST 3: Testing Polling Rate cycling (125 -> 250 -> 500 -> 1000 Hz)...");
  const testRates = [125, 250, 500, 1000];
  for (const rate of testRates) {
    process.stdout.write(`   Testing ${rate} Hz... `);
    const appliedRate = await activeClient.setPollingRate(rate);
    if (appliedRate !== rate) {
      throw new Error(`Failed to set polling rate to ${rate} Hz, got ${appliedRate}`);
    }
    console.log("OK ✅");
    await sleep(DELAY_MS);
  }
  console.log("   ✅ PASSED: All 4 polling rates accepted by mouse hardware.\n");

  // TEST 4: DPI Verification Cycle
  console.log("👉 TEST 4: Testing DPI cycling (400 -> 800 -> 1200 -> 1600 -> 3200 DPI)...");
  const testDpis = [400, 800, 1200, 1600, 3200];
  for (const dpi of testDpis) {
    process.stdout.write(`   Testing ${dpi} DPI... `);
    const appliedDpi = await activeClient.setDpi(dpi);
    if (appliedDpi !== dpi) {
      throw new Error(`Failed to set DPI to ${dpi}, got ${appliedDpi}`);
    }
    console.log("OK ✅");
    await sleep(DELAY_MS);
  }
  console.log("   ✅ PASSED: All DPI levels successfully written to sensor.\n");

  // TEST 5: Active Stage Switching
  console.log("👉 TEST 5: Testing DPI Stage switching...");
  for (let stage = 0; stage < 4; stage++) {
    process.stdout.write(`   Switching to Stage ${stage + 1}... `);
    await activeClient.setActiveDpiStage(stage);
    console.log("OK ✅");
    await sleep(DELAY_MS);
  }
  // Restore stage 2 (1-based index 2 -> 0-based index 1: 1600 DPI)
  await activeClient.setActiveDpiStage(1);
  console.log("   ✅ PASSED: Stage switching operational.\n");

  // TEST 6: Battery Stream Sampling
  console.log("👉 TEST 6: Sampling Battery packet from wireless receiver (3s wait)...");
  let capturedBattery = null;
  const onInputReport = (event) => {
    const data = new Uint8Array(event.data.buffer, event.data.byteOffset, event.data.byteLength);
    if (event.reportId === 0x03 && data.length >= 4 && data[0] === 0x55 && data[1] === 0x40 && data[2] === 0x01) {
      capturedBattery = data[3];
      console.log(`   🔋 Captured live battery level: ${capturedBattery}%`);
    }
  };
  activeDevice.addEventListener("inputreport", onInputReport);
  await sleep(3000);
  activeDevice.removeEventListener("inputreport", onInputReport);
  if (capturedBattery !== null) {
    console.log(`   ✅ PASSED: Battery reporting active (${capturedBattery}%).\n`);
  } else {
    console.log("   ℹ️ NOTE: No battery broadcast received during 3s window (mouse is in idle or powered by cable).\n");
  }

  // TEST 7: Stress Test (Rapid 10 Profile Switches)
  console.log("👉 TEST 7: Stress testing (10 rapid consecutive DPI & Polling Rate writes)...");
  for (let i = 1; i <= 10; i++) {
    const targetDpi = i % 2 === 0 ? 800 : 1600;
    const targetRate = i % 2 === 0 ? 500 : 1000;
    process.stdout.write(`   Round ${i}/10: ${targetDpi} DPI @ ${targetRate} Hz... `);
    await activeClient.setDpi(targetDpi);
    await sleep(150);
    await activeClient.setPollingRate(targetRate);
    await sleep(150);
    console.log("OK ✅");
  }
  console.log("   ✅ PASSED: 10/10 rapid profile changes completed without any errors or disconnects.\n");

  // Cleanly close test client and wait for macOS IOKit to release device handle
  await activeClient.close();
  await sleep(500);

  // TEST 8: End-to-End Bridge Process Test via apply.mjs
  console.log("👉 TEST 8: End-to-end simulation via apply.mjs (exact mechanism Bridge uses)...");
  const { execSync } = await import("child_process");
  const profilePayload = JSON.stringify({ brand: "Delux", dpi: 1600, pollingRateHz: 1000 });
  const result = execSync("node native-hid/src/apply.mjs", {
    input: profilePayload,
    encoding: "utf8",
  });
  console.log("   ✅ PASSED: Bridge apply.mjs exited with code 0.\n");

  console.log("============================================================");
  console.log("🎉 ALL TESTS PASSED! THE DELUX M800 MINI DRIVER IS STABLE! 🎉");
  console.log("============================================================");
}

runTestSuite().catch((err) => {
  console.error("\n❌ TEST FAILED:", err);
  process.exit(1);
});
