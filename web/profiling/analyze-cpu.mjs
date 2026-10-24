#!/usr/bin/env node
// Aggregate a V8 .cpuprofile by self time.
// Usage: node analyze-cpu.mjs out/boot.cpuprofile [topN]

import fs from "node:fs";

const file = process.argv[2];
const topN = Number(process.argv[3] ?? 30);
if (!file) {
  console.error("usage: node analyze-cpu.mjs <profile.cpuprofile> [topN]");
  process.exit(1);
}

const profile = JSON.parse(fs.readFileSync(file, "utf8"));
const nodes = new Map(profile.nodes.map((node) => [node.id, node]));

const selfUs = new Map();
const samples = profile.samples ?? [];
const deltas = profile.timeDeltas ?? [];
for (let i = 0; i < samples.length; i++) {
  const id = samples[i];
  selfUs.set(id, (selfUs.get(id) ?? 0) + (deltas[i] ?? 0));
}

const byFunction = new Map();
const byScript = new Map();
let totalUs = 0;
for (const [id, us] of selfUs) {
  const node = nodes.get(id);
  if (!node) continue;
  totalUs += us;
  const frame = node.callFrame;
  const url = frame.url || "(native)";
  const key = `${frame.functionName || "(anonymous)"} @ ${url}:${frame.lineNumber + 1}`;
  const entry = byFunction.get(key) ?? { us: 0, hits: 0, url, name: frame.functionName || "(anonymous)" };
  entry.us += us;
  entry.hits += 1;
  byFunction.set(key, entry);
  byScript.set(url, (byScript.get(url) ?? 0) + us);
}

const ms = (us) => (us / 1000).toFixed(1);
const pct = (us) => ((us / totalUs) * 100).toFixed(1);
const isWasm = (url) => /\.wasm($|\?)/.test(url);
const isJs = (url) => /\.(js|mjs)($|\?)/.test(url);

const ranked = [...byFunction.entries()].sort((a, b) => b[1].us - a[1].us);
console.log(`\n=== ${file} — total ${(totalUs / 1000).toFixed(0)}ms of samples ===\n`);
console.log("top functions by self time:");
for (const [key, entry] of ranked.slice(0, topN)) {
  const kind = isWasm(entry.url) ? "wasm" : isJs(entry.url) ? "js" : "other";
  console.log(`  ${ms(entry.us).padStart(8)}ms ${pct(entry.us).padStart(5)}%  [${kind}] ${key}`);
}

const wasm = ranked.filter(([, entry]) => isWasm(entry.url));
const wasmTotal = [...byScript.entries()].filter(([url]) => isWasm(url)).reduce((sum, [, us]) => sum + us, 0);
const jsTotal = [...byScript.entries()].filter(([url]) => isJs(url)).reduce((sum, [, us]) => sum + us, 0);
console.log(`\ntotals: wasm ${ms(wasmTotal)}ms (${pct(wasmTotal)}%), js ${ms(jsTotal)}ms (${pct(jsTotal)}%)`);
console.log("\ntop wasm functions by self time:");
for (const [key, entry] of wasm.slice(0, topN)) {
  console.log(`  ${ms(entry.us).padStart(8)}ms ${pct(entry.us).padStart(5)}%  ${key}`);
}

console.log("\ntop scripts by self time:");
for (const [url, us] of [...byScript.entries()].sort((a, b) => b[1] - a[1]).slice(0, 12)) {
  console.log(`  ${ms(us).padStart(8)}ms ${pct(us).padStart(5)}%  ${url}`);
}
