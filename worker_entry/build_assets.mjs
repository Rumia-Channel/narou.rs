// Worker 用の静的アセットを組み立てる。
//
//   node build_assets.mjs
//
// `../src/web/assets` を `./public` へコピーし、ビルド時に:
//   - 各ファイルの内容ハッシュを `?v=<hash>` として HTML / JS の参照に焼き込む
//     (native の `src/web/frontend.rs` と同じ考え方。ランタイムで書き換えない)
//   - `<head>` に `window.__NAROU_RS_WEBUI_BUILD__` を注入する
//   - 全体のハッシュを `.asset-version` に書く (差分検知用)
//
// `public/` は gitignore 済みで、`wrangler` の `[assets]` がそのまま配信する。
import { createHash } from "node:crypto";
import { cp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const workerDir = dirname(fileURLToPath(import.meta.url));
const projectDir = resolve(workerDir, "..");
const sourceDir = join(projectDir, "src", "web", "assets");
const outputDir = join(workerDir, "public");
const assetVersionPath = join(workerDir, ".asset-version");

/** `assets/` 配下の全ファイルを、`/assets/` からの相対パスで列挙する。 */
async function filesUnder(directory, root = directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await filesUnder(path, root)));
    } else if (entry.isFile()) {
      files.push(relative(root, path).split(sep).join("/"));
    }
  }
  return files.sort();
}

function hashOf(buffer) {
  return createHash("sha256").update(buffer).digest("hex").slice(0, 16);
}

/** `from` から `to` への相対モジュール指定子 (native の実装と同じ規則)。 */
function relativeModuleSpecifier(from, to) {
  const fromParts = from.split("/").slice(0, -1);
  const toParts = to.split("/");
  let common = 0;
  while (common < fromParts.length && common < toParts.length && fromParts[common] === toParts[common]) {
    common += 1;
  }
  const parts = [];
  for (let index = common; index < fromParts.length; index += 1) parts.push("..");
  parts.push(...toParts.slice(common));
  return parts[0] === ".." ? parts.join("/") : `./${parts.join("/")}`;
}

/** HTML / JS の参照に `?v=<hash>` を付ける。 */
function applyVersions(source, assetPath, versions, assetPaths) {
  let rendered = source;
  for (const path of assetPaths) {
    const version = versions.get(path);
    if (!version) continue;
    if (assetPath.endsWith(".html")) {
      const asset = `/assets/${path}`;
      rendered = rendered.split(asset).join(`${asset}?v=${version}`);
    }
    if (path.endsWith(".js")) {
      const specifier = relativeModuleSpecifier(assetPath, path);
      const versioned = `${specifier}?v=${version}`;
      rendered = rendered
        .split(`from '${specifier}'`)
        .join(`from '${versioned}'`)
        .split(`from "${specifier}"`)
        .join(`from "${versioned}"`)
        .split(`import('${specifier}')`)
        .join(`import('${versioned}')`)
        .split(`import("${specifier}")`)
        .join(`import("${versioned}")`)
        .split(`import '${specifier}'`)
        .join(`import '${versioned}'`)
        .split(`import "${specifier}"`)
        .join(`import "${versioned}"`);
    }
  }
  return rendered;
}

async function projectVersion() {
  const manifest = await readFile(join(projectDir, "Cargo.toml"), "utf8");
  const match = manifest.match(/^version\s*=\s*"([^"]+)"/m);
  return match ? match[1] : "0.0.0";
}

const assetPaths = await filesUnder(sourceDir);
const contents = new Map();
const versions = new Map();
for (const path of assetPaths) {
  const buffer = await readFile(join(sourceDir, path.split("/").join(sep)));
  contents.set(path, buffer);
  versions.set(path, hashOf(buffer));
}

// 配置: `/assets/...` は native の Axum ルータと同じ URL なので、HTML が持つ
// 参照をそのまま使えるよう `public/assets/` に置く。ページ (`*.html`) だけは
// ルート直下にも置き、`html_handling = "auto-trailing-slash"` で `/settings`
// のような拡張子なしの URL を解決させる。
const assetsDir = join(outputDir, "assets");
await rm(outputDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
await mkdir(assetsDir, { recursive: true });
await cp(sourceDir, assetsDir, { recursive: true });

const buildHash = hashOf(Buffer.from(assetPaths.map((path) => versions.get(path)).join("")));
const version = await projectVersion();
const buildScript = `<script>window.__NAROU_RS_WEBUI_BUILD__ = { appVersion: "${version}", assetsVersion: "${buildHash}" };</script>`;

for (const path of assetPaths) {
  if (!path.endsWith(".html") && !path.endsWith(".js")) continue;
  const original = contents.get(path).toString("utf8");
  let rendered = applyVersions(original, path, versions, assetPaths);
  if (path.endsWith(".html")) {
    rendered = rendered.includes("<head>")
      ? rendered.replace("<head>", `<head>\n    ${buildScript}`)
      : `${buildScript}${rendered}`;
  }
  await writeFile(join(assetsDir, path.split("/").join(sep)), rendered);
  if (path.endsWith(".html") && !path.includes("/")) {
    await writeFile(join(outputDir, path), rendered);
  }
}

await writeFile(assetVersionPath, `${buildHash}\n`);
console.log(`Prepared Worker assets: ${assetPaths.length} files, version ${buildHash}`);
