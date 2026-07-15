import fs from "node:fs";
import path from "node:path";

const version = process.argv[2]?.trim().replace(/^v/, "");
if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error("Usage: npm run release:prepare -- 0.4.7");
  process.exit(1);
}

const root = process.cwd();

function readJson(relativePath) {
  return JSON.parse(fs.readFileSync(path.join(root, relativePath), "utf8"));
}

function writeJson(relativePath, value) {
  fs.writeFileSync(
    path.join(root, relativePath),
    `${JSON.stringify(value, null, 2)}\n`,
  );
}

const packageJson = readJson("package.json");
packageJson.version = version;
writeJson("package.json", packageJson);

const packageLock = readJson("package-lock.json");
packageLock.version = version;
if (packageLock.packages?.[""]) packageLock.packages[""].version = version;
writeJson("package-lock.json", packageLock);

const tauriConfig = readJson("src-tauri/tauri.conf.json");
tauriConfig.version = version;
writeJson("src-tauri/tauri.conf.json", tauriConfig);

const cargoPath = path.join(root, "src-tauri/Cargo.toml");
const cargoToml = fs.readFileSync(cargoPath, "utf8");
const packageSectionEnd = cargoToml.indexOf("\n[", "[package]".length);
const packageSection = cargoToml.slice(0, packageSectionEnd);
const updatedPackageSection = packageSection.replace(
  /(^version\s*=\s*")[^"]+("\s*$)/m,
  `$1${version}$2`,
);
if (updatedPackageSection === packageSection) {
  console.error("Could not update [package].version in src-tauri/Cargo.toml");
  process.exit(1);
}
fs.writeFileSync(
  cargoPath,
  `${updatedPackageSection}${cargoToml.slice(packageSectionEnd)}`,
);

const cargoLockPath = path.join(root, "src-tauri/Cargo.lock");
if (fs.existsSync(cargoLockPath)) {
  const cargoLock = fs.readFileSync(cargoLockPath, "utf8");
  const updatedCargoLock = cargoLock.replace(
    /(\[\[package\]\]\nname = "flipperui"\nversion = ")[^"]+("\n)/,
    `$1${version}$2`,
  );
  if (updatedCargoLock === cargoLock) {
    console.error("Could not update the flipperui package in src-tauri/Cargo.lock");
    process.exit(1);
  }
  fs.writeFileSync(cargoLockPath, updatedCargoLock);
}

const releaseNotesPath = path.join(root, "release-texts", `v${version}.md`);
if (!fs.existsSync(releaseNotesPath)) {
  fs.mkdirSync(path.dirname(releaseNotesPath), { recursive: true });
  fs.writeFileSync(
    releaseNotesPath,
    `# FlipperUI v${version}\n\n## What's new\n\n- TODO\n\n## Fixes\n\n- TODO\n`,
  );
}

console.log(`Prepared FlipperUI v${version}.`);
console.log(`Complete release-texts/v${version}.md, then run the release checks.`);
