import { defineConfig } from 'vite';
import preact from '@preact/preset-vite';

// shelld with the dashboard ([dashboard] port). In dev mode the API is proxied there:
// open http://localhost:5173/_auth?token=… (token from `shellctl dashboard`).
const BACKEND = process.env.WSHELL_DASHBOARD ?? 'http://localhost:8471';

export default defineConfig({
  plugins: [preact()],
  server: {
    proxy: {
      '/api': {
        target: BACKEND,
        changeOrigin: true,
        // shelld checks the Origin of mutating requests: substitute the dashboard origin.
        configure: (proxy) => proxy.on('proxyReq', (req) => req.setHeader('origin', BACKEND)),
      },
      '/_auth': { target: BACKEND, changeOrigin: true },
    },
  },
  build: {
    // No inline scripts or data: URLs: the dashboard CSP is default-src 'self'.
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
  },
});
