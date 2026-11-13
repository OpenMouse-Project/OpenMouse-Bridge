// Run with: node --test .github/scripts/

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  PLATFORM_ASSETS,
  buildLatest,
  changelogEntry,
  versionFromTag,
} from "./make-release-files.mjs";

const CHANGELOG = `# Changelog

Intro text.

## 1.0.1 - 2026-11-20

- Fixed a thing.
- Another fix.

## 1.0.0

- First stable release.

## 1.0.0-beta.21

- A beta.
`;

test("tags become versions", () => {
  assert.equal(versionFromTag("v1.0.0"), "1.0.0");
  assert.equal(versionFromTag("1.2.3-beta.4"), "1.2.3-beta.4");
  assert.throws(() => versionFromTag("dev-build"));
  assert.throws(() => versionFromTag("v1.0"));
});

test("the entry for a version is its section, without neighbours", () => {
  assert.equal(changelogEntry(CHANGELOG, "1.0.1"), "- Fixed a thing.\n- Another fix.");
  assert.equal(changelogEntry(CHANGELOG, "1.0.0"), "- First stable release.");
  // 1.0.0 must not match the 1.0.0-beta.21 heading, nor the other way round.
  assert.equal(changelogEntry(CHANGELOG, "1.0.0-beta.21"), "- A beta.");
  assert.equal(changelogEntry("## [2.0.0]\n- Bracketed.\n", "2.0.0"), "- Bracketed.");
});

test("a missing or empty entry stops the release", () => {
  assert.throws(() => changelogEntry(CHANGELOG, "9.9.9"), /no entry for 9\.9\.9/);
  assert.throws(() => changelogEntry("## 3.0.0\n\n## 2.0.0\n- x\n", "3.0.0"), /empty/);
});

test("latest.json lists every platform with its URL, hash and size", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "bridge-release-"));
  try {
    for (const [index, name] of Object.values(PLATFORM_ASSETS).entries()) {
      writeFileSync(path.join(dir, name), "x".repeat(index + 1));
    }
    const latest = buildLatest({
      tag: "v1.0.1",
      repo: "OpenMouse-Project/OpenMouse-Bridge",
      assetsDir: dir,
      notes: "- Fixed a thing.",
      now: new Date("2026-11-20T12:00:00Z"),
    });
    assert.equal(latest.version, "1.0.1");
    assert.equal(latest.pub_date, "2026-11-20T12:00:00.000Z");
    assert.deepEqual(Object.keys(latest.platforms).sort(), [
      "darwin-universal",
      "linux-x86_64",
      "windows-x86_64",
    ]);
    const linux = latest.platforms["linux-x86_64"];
    assert.equal(
      linux.url,
      "https://github.com/OpenMouse-Project/OpenMouse-Bridge/releases/download/v1.0.1/OpenMouse-Bridge-linux-x86_64.AppImage",
    );
    assert.equal(linux.size, 3);
    assert.equal(linux.sha256, createHash("sha256").update("xxx").digest("hex"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a release missing a platform's asset is refused", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "bridge-release-"));
  try {
    writeFileSync(path.join(dir, PLATFORM_ASSETS["windows-x86_64"]), "x");
    assert.throws(
      () => buildLatest({ tag: "v1.0.1", repo: "o/r", assetsDir: dir, notes: "- x" }),
      /missing openmouse-bridge-macos-app\.zip/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("platform keys match what the updater looks for", () => {
  const feed = readFileSync(
    path.join(path.dirname(fileURLToPath(import.meta.url)), "../../src/updater/feed.rs"),
    "utf8",
  );
  for (const key of Object.keys(PLATFORM_ASSETS)) {
    assert.ok(feed.includes(`"${key}"`), `src/updater/feed.rs does not use ${key}`);
  }
});

test("this repository's changelog has an entry for Cargo.toml's version", () => {
  const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "../..");
  const cargo = readFileSync(path.join(root, "Cargo.toml"), "utf8");
  const version = cargo.match(/^version = "(.+)"$/m)[1];
  const entry = changelogEntry(readFileSync(path.join(root, "CHANGELOG.md"), "utf8"), version);
  // Entries are bullets, or bullets under `###` headings (New, Improvements, Fixes).
  assert.ok(/^(- |### )/.test(entry));
});
