// Writes the two files a release publishes besides its build artifacts:
//
//   latest.json  what Bridge's update check reads
//                (releases/latest/download/latest.json): the version, its
//                notes, and per platform the asset's URL, SHA-256 and size,
//                so the in-app updater can verify what it downloaded.
//   notes.md     the release description on GitHub.
//
// The notes are the version's entry in CHANGELOG.md, so the release page and
// the update the app offers always say the same thing. A version without an
// entry is not released.
//
// Usage: node make-release-files.mjs <tag> <repo> <assets-dir> <out-dir>
//   e.g. node make-release-files.mjs v1.0.0 OpenMouse-Project/OpenMouse-Bridge artifacts release

import { createHash } from "node:crypto";
import { readFileSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** The asset each platform key in latest.json points at (see src/updater/feed.rs). */
export const PLATFORM_ASSETS = {
  "windows-x86_64": "OpenMouse-Bridge-windows-x64-setup.exe",
  "darwin-universal": "openmouse-bridge-macos-app.zip",
  "linux-x86_64": "OpenMouse-Bridge-linux-x86_64.AppImage",
};

/** `v1.2.3` (or `1.2.3`) as a semver version string, or an error. */
export function versionFromTag(tag) {
  const version = tag.replace(/^v/, "");
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`tag ${tag} is not a version like v1.2.3`);
  }
  return version;
}

/**
 * The body of `## <version>` in a changelog: everything up to the next `## `
 * heading, trimmed. A heading may add a date after the version
 * (`## 1.0.0 - 2026-11-20`). Throws when there is no entry or it is empty.
 */
export function changelogEntry(changelog, version) {
  const lines = changelog.split(/\r?\n/);
  const heading = new RegExp(`^## \\[?${version.replace(/[.+-]/g, "\\$&")}\\]?(\\s|$)`);
  const start = lines.findIndex((line) => heading.test(line));
  if (start < 0) {
    throw new Error(`CHANGELOG.md has no entry for ${version}; add one before releasing.`);
  }
  const rest = lines.slice(start + 1);
  const end = rest.findIndex((line) => line.startsWith("## "));
  const body = (end < 0 ? rest : rest.slice(0, end)).join("\n").trim();
  if (!body) {
    throw new Error(`CHANGELOG.md's entry for ${version} is empty.`);
  }
  return body;
}

/** latest.json for `tag`, from the release assets found in `assetsDir`. */
export function buildLatest({ tag, repo, assetsDir, notes, now = new Date() }) {
  const version = versionFromTag(tag);
  const platforms = {};
  for (const [key, name] of Object.entries(PLATFORM_ASSETS)) {
    const file = path.join(assetsDir, name);
    let bytes;
    try {
      bytes = readFileSync(file);
    } catch {
      throw new Error(`the release is missing ${name}, which latest.json needs for ${key}`);
    }
    platforms[key] = {
      url: `https://github.com/${repo}/releases/download/${tag}/${name}`,
      sha256: createHash("sha256").update(bytes).digest("hex"),
      size: statSync(file).size,
    };
  }
  return { version, notes, pub_date: now.toISOString(), platforms };
}

function main() {
  const [tag, repo, assetsDir, outDir] = process.argv.slice(2);
  if (!tag || !repo || !assetsDir || !outDir) {
    console.error("usage: make-release-files.mjs <tag> <repo> <assets-dir> <out-dir>");
    process.exit(2);
  }
  const version = versionFromTag(tag);
  const notes = changelogEntry(readFileSync("CHANGELOG.md", "utf8"), version);
  const latest = buildLatest({ tag, repo, assetsDir, notes });
  writeFileSync(path.join(outDir, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);
  writeFileSync(path.join(outDir, "notes.md"), `${notes}\n`);
  console.log(`wrote latest.json and notes.md for ${version}`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  try {
    main();
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
}
