#!/usr/bin/env node
// Analyze CDP memory artifacts:
//   node analyze-memory.mjs alloc out/load-10k.allocprofile.json [topN]
//   node analyze-memory.mjs heap  out/load-10k.heapsnapshot [topN]

import fs from "node:fs";

const [mode, file, topNArg] = process.argv.slice(2);
const topN = Number(topNArg ?? 25);
if (!mode || !file) {
  console.error("usage: node analyze-memory.mjs <alloc|heap> <file> [topN]");
  process.exit(1);
}

const mb = (bytes) => (bytes / 1048576).toFixed(1);

if (mode === "alloc") {
  const payload = JSON.parse(fs.readFileSync(file, "utf8"));
  const root = payload.profile?.head ?? payload.head;
  const byFrame = new Map();
  let total = 0;
  const walk = (node) => {
    const frame = node.callFrame ?? {};
    const key = `${frame.functionName || "(anonymous)"} @ ${frame.url || "(native)"}:${(frame.lineNumber ?? -1) + 1}`;
    const entry = byFrame.get(key) ?? { size: 0, count: 0 };
    entry.size += node.selfSize ?? 0;
    entry.count += 1;
    byFrame.set(key, entry);
    total += node.selfSize ?? 0;
    for (const child of node.children ?? []) walk(child);
  };
  walk(root);
  console.log(`\n=== ${file} — sampled allocations ${mb(total)}MB total ===\n`);
  for (const [key, entry] of [...byFrame.entries()].sort((a, b) => b[1].size - a[1].size).slice(0, topN)) {
    console.log(`  ${mb(entry.size).padStart(8)}MB  (${String(entry.count).padStart(6)} sites)  ${key}`);
  }
} else if (mode === "heap") {
  const parsed = JSON.parse(fs.readFileSync(file, "utf8"));
  const meta = parsed.snapshot?.meta ?? parsed.meta;
  const nodes = parsed.nodes;
  const strings = parsed.strings;
  const fields = meta.node_fields;
  const stride = fields.length;
  const typeIndex = fields.indexOf("type");
  const nameIndex = fields.indexOf("name");
  const sizeIndex = fields.indexOf("self_size");
  const byName = new Map();
  let total = 0;
  for (let i = 0; i < nodes.length; i += stride) {
    const type = meta.node_types[typeIndex][nodes[i + typeIndex]];
    const rawName = strings[nodes[i + nameIndex]] || "";
    const name = rawName.length > 60 ? rawName.slice(0, 57) + "..." : rawName;
    const size = nodes[i + sizeIndex];
    total += size;
    const key = `${type}:${name || "(unnamed)"}`;
    const entry = byName.get(key) ?? { size: 0, count: 0 };
    entry.size += size;
    entry.count += 1;
    byName.set(key, entry);
  }
  console.log(`\n=== ${file} — heap ${mb(total)}MB across ${(nodes.length / stride).toLocaleString()} nodes ===\n`);
  for (const [key, entry] of [...byName.entries()].sort((a, b) => b[1].size - a[1].size).slice(0, topN)) {
    console.log(`  ${mb(entry.size).padStart(8)}MB  x${String(entry.count).padStart(7)}  ${key}`);
  }
} else {
  console.error(`unknown mode: ${mode}`);
  process.exit(1);
}
