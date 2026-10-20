import react from '@vitejs/plugin-react'
import { defineConfig, loadEnv, type Plugin } from 'vite'

const csp = (dev: boolean) =>
  [
    "default-src 'self'",
    "img-src 'self' data:",
    "style-src 'self' 'unsafe-inline'",
    dev ? "script-src 'self' 'unsafe-inline'" : "script-src 'self'",
    dev ? "connect-src 'self' ws:" : "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'self'",
  ].join('; ')

const cspPlugin = (): Plugin => ({
  name: 'csp-meta',
  transformIndexHtml: {
    order: 'pre',
    handler: (_html, ctx) => [
      {
        tag: 'meta',
        attrs: { 'http-equiv': 'Content-Security-Policy', content: csp(!!ctx.server) },
        injectTo: 'head-prepend',
      },
    ],
  },
})

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '')
  const target = env.GATEWAY_URL || env.VITE_GATEWAY_URL || 'http://localhost:8080'
  const proxy = Object.fromEntries(
    ['/auth', '/v1', '/admin'].map((p) => [p, { target, changeOrigin: true }]),
  )
  return {
    plugins: [react(), cspPlugin()],
    server: { proxy },
  }
})
