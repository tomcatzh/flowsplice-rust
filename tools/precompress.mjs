import { brotliCompressSync, constants, gzipSync } from "node:zlib";
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = process.argv[2];
if (!root) throw new Error("usage: node tools/precompress.mjs <directory>");
const extensions = new Set([".css", ".html", ".js", ".json", ".svg", ".txt", ".webmanifest", ".xml"]);

function visit(directory) {
  for (const name of readdirSync(directory)) {
    const path = join(directory, name);
    if (statSync(path).isDirectory()) {
      visit(path);
      continue;
    }
    const dot = name.lastIndexOf(".");
    if (dot < 0 || !extensions.has(name.slice(dot))) continue;
    const input = readFileSync(path);
    writeFileSync(`${path}.gz`, gzipSync(input, { level: 9 }));
    writeFileSync(`${path}.br`, brotliCompressSync(input, {
      params: { [constants.BROTLI_PARAM_QUALITY]: 11 },
    }));
  }
}

visit(root);
