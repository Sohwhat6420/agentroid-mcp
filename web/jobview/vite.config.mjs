// Vite config for the job view. bridged serves the built dist/ from its own origin, so every URL is
// relative (base './') and nothing is fetched from a third party. The dev server binds 127.0.0.1 only.
import { defineConfig } from 'vite';
import { fileURLToPath } from 'node:url';

const scaffold = fileURLToPath(new URL('../..', import.meta.url));

export default defineConfig({
  base: './',
  server: { host: '127.0.0.1', port: 5179, strictPort: true, fs: { allow: [scaffold] } },
  preview: { host: '127.0.0.1', port: 5180, strictPort: true },
  build: {
    outDir: 'dist', emptyOutDir: true, target: 'es2022', assetsInlineLimit: 0, sourcemap: false,
    modulePreload: { polyfill: false },
  },
});
