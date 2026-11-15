import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, watch: { ignored: ['**/src-tauri/**'] } },
  build: { target: 'safari16', sourcemap: false, chunkSizeWarningLimit: 4096 },
  worker: { format: 'es' },
  test: {
    environment: 'jsdom',
    // The first PDF test loads pdf.js; on a cold CI machine that alone can pass 5 s.
    testTimeout: 30000,
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
