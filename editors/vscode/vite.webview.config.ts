/**
 * Webview build: webview/graph.html (+ graph.js) -> out/webview/.
 * assetsInlineLimit: 0 is critical — VS Code webview CSP forbids inlined
 * scripts, so JS must stay an external file referenced by src.
 */
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';

// Root is the webview/ directory so the HTML entry lands at
// out/webview/graph.html (not out/webview/webview/graph.html).
const webviewRoot = fileURLToPath(new URL('webview', import.meta.url));
const outDir = fileURLToPath(new URL('out/webview', import.meta.url));

export default defineConfig({
  root: webviewRoot,
  build: {
    outDir,
    target: 'es2022',
    sourcemap: true,
    assetsInlineLimit: 0,
    emptyOutDir: true,
    rollupOptions: {
      input: path.join(webviewRoot, 'graph.html'),
    },
  },
});
