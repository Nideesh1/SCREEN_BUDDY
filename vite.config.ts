import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      // Never watch the Rust side. `tauri dev` runs cargo concurrently with
      // this dev server, and cargo churns through thousands of files in
      // src-tauri/target — including build-script .exe files it holds open.
      //
      // On Windows an open file is LOCKED, so chokidar's attempt to watch one
      // mid-write throws EBUSY and takes the whole dev server down with it
      // ("beforeDevCommand terminated with a non-zero status code"). Unix lets
      // you watch a file being written, which is why this never fired on macOS.
      //
      // Watching Rust output was never useful anyway — cargo already rebuilds
      // and Tauri restarts the app itself.
      ignored: ['**/src-tauri/**'],
    },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: ['es2021', 'chrome97', 'safari13'],
    minify: !process.env.TAURI_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_DEBUG,
  },
})
