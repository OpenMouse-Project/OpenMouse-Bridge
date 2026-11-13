import assert from "node:assert/strict";
import test from "node:test";
import { addEntry, bumpManifest, nextPatch, sections } from "./weekly-release.mjs";

test("nextPatch bumps the patch number", () => {
  assert.equal(nextPatch("1.0.9"), "1.0.10");
  assert.throws(() => nextPatch("1.0.0-beta.1"));
});

test("sections group user-facing commits and drop the rest", () => {
  assert.equal(
    sections([
      "fix(linux): use system notifications on Wayland (#101)",
      "ci: retry downloads (#96)",
      "fix(hid): never offer FIDO security keys",
      "feat(native-hid): add a thing!",
      "perf: faster start",
      "revert: undo it",
      "docs: update the readme",
      "not conventional",
      "fix(linux): use system notifications on Wayland (#102)",
    ]),
    [
      "### New\n\n- Add a thing!",
      "### Improvements\n\n- Faster start.",
      "### Fixes\n\n- Linux: Use system notifications on Wayland.\n- Never offer FIDO security keys.",
      "### Other\n\n- Undo it.",
    ].join("\n\n"),
  );
  assert.equal(sections(["ci: x", "chore: y"]), "");
});

test("addEntry goes above the newest entry", () => {
  const out = addEntry("# Changelog\n\nintro\n\n## 1.0.0\n\n- a\n", "1.0.1", "### Fixes\n\n- b.");
  assert.match(out, /intro\n\n## 1\.0\.1\n\n### Fixes\n\n- b\.\n\n## 1\.0\.0\n\n- a/);
});

test("bumpManifest touches only Bridge's own version", () => {
  const { toml, lock } = bumpManifest(
    'name = "x"\nversion = "1.0.0"\n',
    'name = "other"\nversion = "1.0.0"\n\nname = "openmouse-bridge"\nversion = "1.0.0"\n',
    "1.0.1",
  );
  assert.equal(toml, 'name = "x"\nversion = "1.0.1"\n');
  assert.match(lock, /name = "other"\nversion = "1.0.0"/);
  assert.match(lock, /name = "openmouse-bridge"\nversion = "1.0.1"/);
});
