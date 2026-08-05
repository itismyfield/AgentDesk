#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const dashboardDir = path.resolve(process.argv[2] ?? path.join(import.meta.dirname, "..", "dashboard"));
const manifestPath = path.join(dashboardDir, "package.json");
const lockPath = path.join(dashboardDir, "package-lock.json");
const installedLockPath = path.join(dashboardDir, "node_modules", ".package-lock.json");

function fail(message) {
  console.error(`Error: dashboard dependency cache is not reusable: ${message}`);
  process.exit(1);
}

function readJson(filePath, label) {
  try {
    return JSON.parse(fs.readFileSync(filePath, "utf8"));
  } catch (error) {
    fail(`${label} is missing or invalid (${error.message})`);
  }
}

function stableMap(value) {
  return Object.fromEntries(Object.entries(value ?? {}).sort(([left], [right]) => left.localeCompare(right)));
}

function sameMap(left, right) {
  return JSON.stringify(stableMap(left)) === JSON.stringify(stableMap(right));
}

const manifest = readJson(manifestPath, "package.json");
const lock = readJson(lockPath, "package-lock.json");
const installedLock = readJson(installedLockPath, "node_modules/.package-lock.json");
const desiredPackages = lock.packages;
const installedPackages = installedLock.packages;

if (!desiredPackages || !desiredPackages[""] || !installedPackages) {
  fail("npm lock metadata does not contain the package tree required for an exact comparison");
}
if (lock.lockfileVersion !== installedLock.lockfileVersion) {
  fail(`lockfile version ${installedLock.lockfileVersion ?? "unknown"} does not match ${lock.lockfileVersion ?? "unknown"}`);
}

const rootLock = desiredPackages[""];
for (const group of ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"]) {
  if (!sameMap(manifest[group], rootLock[group])) {
    fail(`package.json ${group} do not match package-lock.json`);
  }
}

for (const [packagePath, expected] of Object.entries(desiredPackages)) {
  if (!packagePath || expected.optional === true) {
    continue;
  }
  if (!installedPackages[packagePath]) {
    fail(`required package ${packagePath} is missing from the installed lock`);
  }
}

for (const [packagePath, installed] of Object.entries(installedPackages)) {
  if (!packagePath) {
    continue;
  }
  const expected = desiredPackages[packagePath];
  if (!expected) {
    fail(`installed package ${packagePath} is not present in package-lock.json`);
  }
  for (const field of ["version", "resolved", "integrity", "link"]) {
    if ((installed[field] ?? null) !== (expected[field] ?? null)) {
      fail(`${packagePath} ${field} does not match package-lock.json`);
    }
  }
  if (expected.link === true) {
    continue;
  }
  const installedManifest = readJson(path.join(dashboardDir, packagePath, "package.json"), `${packagePath}/package.json`);
  if ((installedManifest.version ?? null) !== (expected.version ?? null)) {
    fail(`${packagePath} on-disk version does not match package-lock.json`);
  }
}

console.log(`Dashboard dependency cache matches package-lock.json (${Object.keys(installedPackages).length} installed package entries)`);
