// Prepares the weekly stable release: works out the next patch version from the
// merged commits since the last stable tag, writes its CHANGELOG.md entry, and
// bumps Cargo.toml and Cargo.lock.
//
// Usage: node weekly-release.mjs <last-stable-tag> <subjects-file>
//   <subjects-file> holds one commit subject per line (git log --format=%s).
// Prints `version=<x.y.z>` when there is something to release, nothing when
// the week had no user-facing changes.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

/** Commit types that never reach users, so they alone do not justify a release. */
const HIDDEN = new Set(["chore", "ci", "docs", "test", "build", "style", "refactor"]);

/** `1.2.3` -> `1.2.4`. */
export function nextPatch(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!match) throw new Error(`${version} is not a plain x.y.z version`);
  return `${match[1]}.${match[2]}.${Number(match[3]) + 1}`;
}

/** Changelog bullets for the user-facing commit subjects, oldest first. */
export function bullets(subjects) {
  const seen = new Set();
  const lines = [];
  for (const subject of subjects) {
    const match = /^(\w+)(?:\(([^)]*)\))?!?:\s*(.+?)(?:\s+\(#\d+\))?$/.exec(subject.trim());
    if (!match || HIDDEN.has(match[1])) continue;
    const [, , scope, text] = match;
    const sentence = text.charAt(0).toUpperCase() + text.slice(1);
    const line = `- ${scope ? `${scope[0].toUpperCase()}${scope.slice(1)}: ` : ""}${sentence}${/[.!?]$/.test(sentence) ? "" : "."}`;
    if (!seen.has(line)) {
      seen.add(line);
      lines.push(line);
    }
  }
  return lines;
}

/** `changelog` with `## <version>` added above the newest entry. */
export function addEntry(changelog, version, lines) {
  const entry = `## ${version}\n\n${lines.join("\n")}\n\n`;
  const at = changelog.search(/^## /m);
  if (at < 0) return `${changelog.trimEnd()}\n\n${entry}`;
  return changelog.slice(0, at) + entry + changelog.slice(at);
}

/** Cargo.toml and Cargo.lock text with Bridge's own version replaced. */
export function bumpManifest(toml, lock, version) {
  return {
    toml: toml.replace(/^version = ".*"$/m, `version = "${version}"`),
    lock: lock.replace(
      /(name = "openmouse-bridge"\nversion = )".*"/,
      `$1"${version}"`,
    ),
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [tag, subjectsFile] = process.argv.slice(2);
  const subjects = readFileSync(subjectsFile, "utf8").split("\n").filter(Boolean);
  const lines = bullets(subjects);
  if (lines.length === 0) {
    console.error("No user-facing changes since", tag);
  } else {
    const version = nextPatch(tag.replace(/^v/, ""));
    writeFileSync("CHANGELOG.md", addEntry(readFileSync("CHANGELOG.md", "utf8"), version, lines));
    const { toml, lock } = bumpManifest(
      readFileSync("Cargo.toml", "utf8"),
      readFileSync("Cargo.lock", "utf8"),
      version,
    );
    writeFileSync("Cargo.toml", toml);
    writeFileSync("Cargo.lock", lock);
    console.log(`version=${version}`);
  }
}
