/**
 * Extension host build: src/extension.ts (ESM sources) -> out/extension.js (CJS).
 * 'vscode' and node builtins stay external; target node20.
 */
import { defineConfig } from 'vite';

export default defineConfig({
  build: {
    outDir: 'out',
    target: 'node20',
    sourcemap: true,
    minify: false,
    lib: {
      entry: 'src/extension.ts',
      formats: ['cjs'],
      fileName: () => 'extension.js',
    },
    rollupOptions: {
      external: (id) => id === 'vscode' || id.startsWith('node:'),
    },
  },
});
