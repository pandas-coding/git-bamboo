#!/usr/bin/env node
/**
 * Build wrapper: runs both vite builds (extension host + webview).
 * Usage: node build.js [--watch]
 * CommonJS on purpose — package.json has no "type": "module" (out/extension.js
 * must stay CJS for the VS Code extension host).
 */
'use strict';

const { spawn } = require('node:child_process');

const watch = process.argv.includes('--watch');
const npx = process.platform === 'win32' ? 'npx.cmd' : 'npx';

const builds = [
  { label: 'extension host', config: 'vite.config.ts' },
  { label: 'webview', config: 'vite.webview.config.ts' },
];

function runVite(build) {
  const args = ['vite', 'build', '--config', build.config];
  if (watch) args.push('--watch');
  const child = spawn(npx, args, {
    stdio: 'inherit',
    shell: process.platform === 'win32',
  });
  return new Promise((resolve) => {
    child.on('error', (err) => {
      console.error(`failed to start vite for ${build.config}: ${err.message}`);
      resolve(1);
    });
    child.on('exit', (code) => resolve(code == null ? 1 : code));
  });
}

async function main() {
  if (watch) {
    // Both watchers run concurrently until killed.
    await Promise.all(builds.map(runVite));
    return;
  }
  for (const build of builds) {
    console.log(`==> building ${build.label} (${build.config})`);
    const code = await runVite(build);
    if (code !== 0) {
      console.error(`build failed: ${build.label} (${build.config})`);
      process.exit(code);
    }
  }
  console.log('==> all builds succeeded');
}

main();
