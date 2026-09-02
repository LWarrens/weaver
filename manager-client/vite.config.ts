import { defineConfig, loadEnv } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const target = env.VITE_MCP_TARGET || 'http://127.0.0.1:8444';

  return {
    plugins: [svelte()],
    server: {
      port: 4173,
      proxy: {
        '/mcp': {
          target,
          changeOrigin: true,
        },
      },
    },
  };
});
