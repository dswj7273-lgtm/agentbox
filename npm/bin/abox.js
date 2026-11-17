#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const PLATFORM_PACKAGES = {
  "darwin-arm64": "@agentbox/darwin-arm64",
  "darwin-x64": "@agentbox/darwin-x64",
  "linux-x64": "@agentbox/linux-x64",
  "linux-arm64": "@agentbox/linux-arm64",
};

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PLATFORM_PACKAGES[key];
  if (pkg) {
    try {
      const binPath = require.resolve(`${pkg}/bin/abox`);
      if (fs.existsSync(binPath)) return binPath;
    } catch (_) {
      // Optional dependency not installed; check local repo / PATH fallback
    }
  }

  const localRelease = path.resolve(__dirname, "..", "..", "target", "release", "abox");
  if (fs.existsSync(localRelease)) return localRelease;

  return "abox";
}

const bin = resolveBinary();
const res = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (res.error) {
  console.error(`Failed to launch abox (${bin}): ${res.error.message}`);
  console.error("Build locally with `cargo build --release` or install via Homebrew.");
  process.exit(1);
}
process.exit(res.status ?? 0);
