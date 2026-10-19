#!/usr/bin/env node
/**
 * Package a platform-specific VSIX. A package without TargetPlatform is a
 * universal fallback and cannot be uploaded as an OS-specific build.
 *
 * Default target is the host (linux-x64, darwin-arm64, win32-x64, ...).
 * `npm run package -- --target win32-x64` still overrides it.
 */
'use strict';

const { spawnSync } = require('node:child_process');

const TARGETS = {
  'linux-x64': 'linux-x64',
  'linux-arm64': 'linux-arm64',
  'darwin-x64': 'darwin-x64',
  'darwin-arm64': 'darwin-arm64',
  'win32-x64': 'win32-x64',
  'win32-arm64': 'win32-arm64',
};

function hostTarget() {
  const key = `${process.platform}-${process.arch}`;
  const target = TARGETS[key];
  if (!target) {
    console.error(`no VS Code target for ${key}; pass --target explicitly`);
    process.exit(1);
  }
  return target;
}

const extra = process.argv.slice(2);
const args = ['vsce', 'package'];
if (!extra.includes('--target')) {
  args.push('--target', hostTarget());
}
args.push(...extra);

const npx = process.platform === 'win32' ? 'npx.cmd' : 'npx';
const result = spawnSync(npx, args, {
  stdio: 'inherit',
  shell: process.platform === 'win32',
});
process.exit(result.status == null ? 1 : result.status);
