// SCRATCH (never landed): runs the repo's acpmux protocol codegen on a fleet
// host and packs its output as the step artifact.
import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";

test("scratch: regenerate the acpmux protocol web types", () => {
  const webviews = path.resolve(import.meta.dir, "../../..");
  const repo = path.resolve(webviews, "..");
  // The fleet test's fresh IR, put where the codegen reads it (scratch only).
  copyFileSync(
    path.join(import.meta.dir, "zz-scratch-acpmux-protocol.json"),
    path.join(repo, "cmux-tui/crates/acpmux/spec/acpmux-protocol.json"),
  );
  const gen = spawnSync(process.execPath, ["scripts/acpmux-protocol-codegen.ts"], { cwd: webviews, encoding: "utf8" });
  console.log(gen.stdout, gen.stderr);
  expect(gen.status).toBe(0);
  mkdirSync(path.join(repo, ".build"), { recursive: true });
  const tar = spawnSync(
    "tar",
    ["-czf", path.join(repo, ".build/acpmux-generated.tgz"), "src/agent-session/acpmux/protocol/generated"],
    { cwd: webviews, encoding: "utf8" },
  );
  console.log(tar.stdout, tar.stderr);
  expect(tar.status).toBe(0);
});
