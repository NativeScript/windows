#!/usr/bin/env node
// Resolve the release identity for the npm_release workflow's `setup` job and write it to
// $GITHUB_OUTPUT: NPM_VERSION, NPM_TAG, BUILD_MATRIX.
//
// Trigger shapes:
//   * workflow_dispatch with a version -> that version (leading "v" allowed)
//   * push of a v* tag                 -> version from the tag, which is authoritative: every
//     selected package is stamped to it (the packages carry their own package.json versions, so
//     there is no single manifest to cross-check the tag against)
//   * workflow_dispatch without a version -> rolling "next" prerelease
//
// One version covers every package in a run: the classic runtime and the four engine variants are
// the same framework built from the same commit and differ only in the runtime DLL, so they
// release in lockstep.
//
// The repo has no root package.json, so this stays dependency-free — no semver/dayjs, and no
// `npm install` step in front of it.
//
// GITHUB_REF, GITHUB_RUN_ID and GITHUB_OUTPUT are read from the environment (GitHub's contract);
// the workflow_dispatch inputs are real arguments.
import fs from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { fileURLToPath } from "node:url";

// Every publishable package, keyed by the `engine` the workflow matrix runs on. `classic` is the
// workspace `nativescript` cdylib (rusty_v8) published from template/; the rest are the napi engine
// packages, each staged from the same template scaffolding by `template/build.ps1 -Engine`.
const PACKAGES = {
  classic: { dir: "template", name: "@nativescript/windows" },
  hermes: { dir: "packages/windows-hermes", name: "@nativescript/windows-hermes" },
  jsc: { dir: "packages/windows-jsc", name: "@nativescript/windows-jsc" },
  quickjs: { dir: "packages/windows-quickjs", name: "@nativescript/windows-quickjs" },
  v8: { dir: "packages/windows-v8", name: "@nativescript/windows-v8" },
};
const ENGINES = Object.keys(PACKAGES);

const USAGE = `Usage: node scripts/resolve-release.mjs [--version <version>] [--engine <engine>] [--skip-arm64]

  --version     release version to cut (leading "v" allowed); empty or omitted resolves from
                GITHUB_REF (v* tag) or falls back to a rolling "next" prerelease
  --engine      all (default) | ${ENGINES.join(" | ")}
  --skip-arm64  build the classic package x64-only (the engine variants are x64-only regardless)
  -h, --help    show this help`;

let values;
try {
  ({ values } = parseArgs({
    options: {
      version: { type: "string", default: "" },
      engine: { type: "string", default: "all" },
      "skip-arm64": { type: "boolean", default: false },
      help: { type: "boolean", short: "h", default: false },
    },
  }));
} catch (e) {
  console.error(e.message);
  console.error(USAGE);
  process.exit(1);
}
if (values.help) {
  console.log(USAGE);
  process.exit(0);
}

const repoRoot = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");

const SEMVER = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/;

function parseVersion(version, what) {
  const m = SEMVER.exec(version);
  if (!m) {
    console.error(`::error::${what} is not a valid semver version: ${version}`);
    process.exit(1);
  }
  return {
    major: Number(m[1]),
    minor: Number(m[2]),
    patch: Number(m[3]),
    prerelease: m[4] ? m[4].split(".") : [],
  };
}

// Precedence by release core only, with a plain release outranking any prerelease of the same core.
// That is all the rolling-version base needs: identifier-level ordering (alpha.1 vs alpha.2) can
// never change the core it derives from.
function isNewer(a, b) {
  for (const part of ["major", "minor", "patch"]) {
    if (a[part] !== b[part]) return a[part] > b[part];
  }
  return b.prerelease.length > 0 && a.prerelease.length === 0;
}

// The npm dist-tag is the version's first prerelease identifier ("alpha"/"beta"/"next"), or
// "latest" for a plain release. A non-alphabetic identifier (1.0.0-1) would yield a nonsense tag.
function npmTag(parsed, version) {
  const [id] = parsed.prerelease;
  if (id === undefined) return "latest";
  if (!/^[a-zA-Z]+$/.test(id)) {
    console.error(`::error::Version ${version} has no usable npm dist-tag (prerelease "${id}").`);
    process.exit(1);
  }
  return id;
}

const engine = values.engine.trim() || "all";
if (engine !== "all" && !ENGINES.includes(engine)) {
  console.error(`::error::Unknown engine "${engine}". Expected: all, ${ENGINES.join(", ")}`);
  process.exit(1);
}
const engines = engine === "all" ? ENGINES : [engine];

const inputVersion = values.version.trim();
const ref = process.env.GITHUB_REF || "";

let version;
if (inputVersion) {
  version = inputVersion.replace(/^v/, "");
} else if (ref.startsWith("refs/tags/")) {
  version = ref.slice("refs/tags/".length).replace(/^v/, "");
} else {
  // Rolling prerelease off the highest version the selected packages currently declare. A base
  // that is already released takes a patch bump, so the "next" channel never trails "latest".
  let base;
  for (const e of engines) {
    const manifest = path.join(repoRoot, PACKAGES[e].dir, "package.json");
    const declared = parseVersion(
      JSON.parse(fs.readFileSync(manifest, "utf8")).version,
      `${PACKAGES[e].dir}/package.json version`
    );
    if (!base || isNewer(declared, base)) base = declared;
  }
  const patch = base.prerelease.length ? base.patch : base.patch + 1;
  const date = new Date().toISOString().slice(0, 10);
  version = `${base.major}.${base.minor}.${patch}-next.${date}-${process.env.GITHUB_RUN_ID || 0}`;
}

const tag = npmTag(parseVersion(version, "Release version"), version);

// One matrix entry per package. Besides the engine key the workflow needs, for each package:
//   dir              where package.json lives (npm version / npm pack run there)
//   npm_name         the published name
//   tarball          what `npm pack` emits ("@scope/name" -> "scope-name-<version>.tgz")
//   rust_targets     extra Rust targets the toolchain must install (classic cross-compiles arm64)
//   rust_workspaces  the Swatinem/rust-cache `workspaces` mapping; the engine packages are excluded
//                    from the root workspace and build into their own target/ dir
const matrix = {
  include: engines.map((e) => {
    const pkg = PACKAGES[e];
    const classic = e === "classic";
    return {
      engine: e,
      dir: pkg.dir,
      npm_name: pkg.name,
      tarball: `${pkg.name.replace(/^@/, "").replace("/", "-")}-${version}.tgz`,
      rust_targets: classic
        ? values["skip-arm64"]
          ? "x86_64-pc-windows-msvc"
          : "x86_64-pc-windows-msvc,aarch64-pc-windows-msvc"
        : "",
      rust_workspaces: classic ? ". -> target" : `. -> target\n${pkg.dir} -> target`,
    };
  }),
};

if (process.env.GITHUB_OUTPUT) {
  fs.appendFileSync(
    process.env.GITHUB_OUTPUT,
    `NPM_VERSION=${version}\nNPM_TAG=${tag}\nBUILD_MATRIX=${JSON.stringify(matrix)}\n`
  );
}
console.log(
  `Resolved ${version} (dist-tag: ${tag}) for ${engines.map((e) => PACKAGES[e].name).join(", ")}`
);
