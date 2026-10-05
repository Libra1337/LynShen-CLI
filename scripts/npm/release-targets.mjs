export const releaseTargets = [
  {
    directory: "cli-linux-x64",
    packageName: "@lynshen/cli-linux-x64",
    rustTarget: "x86_64-unknown-linux-gnu",
    binaryName: "lynshen"
  },
  {
    directory: "cli-win32-x64",
    packageName: "@lynshen/cli-win32-x64",
    rustTarget: "x86_64-pc-windows-msvc",
    binaryName: "lynshen.exe"
  },
  {
    directory: "cli-darwin-arm64",
    packageName: "@lynshen/cli-darwin-arm64",
    rustTarget: "aarch64-apple-darwin",
    binaryName: "lynshen"
  },
  {
    directory: "cli-darwin-x64",
    packageName: "@lynshen/cli-darwin-x64",
    rustTarget: "x86_64-apple-darwin",
    binaryName: "lynshen"
  }
];

// Which targets each workflow builds/publishes. macOS lives in its own workflow so
// its slower runners never block the linux/windows release.
export const nativeReleaseTargets = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"];
export const macosReleaseTargets = ["aarch64-apple-darwin"];

export const rootPackageDirectory = "cli";
export const rootPackageName = "@lynshen/cli";
