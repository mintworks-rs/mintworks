import tailwindcss from '@tailwindcss/postcss'
import esbuild from 'esbuild'
import stylePlugin from 'esbuild-style-plugin'
import { rmSync } from 'fs'
import { cp, mkdir } from 'fs/promises'
import path from 'path'
import { fileURLToPath } from 'url'

const rootDir = path.dirname(fileURLToPath(import.meta.url))
const distDir = path.join(rootDir, 'dist')
const isProd = process.env.NODE_ENV === 'production'

// index.html is copied, not templated: assets are unversioned here, so there is
// nothing to substitute. It is copied once per invocation, so `watch` does not
// pick up edits to it — restart the watcher after touching it.
// Wiped first: without it a dev build's full-source app.js.map survived beside the
// next production app.js, and ServeDir published the whole source at /app.js.map.
rmSync(distDir, { recursive: true, force: true })
await mkdir(distDir, { recursive: true })
await cp(path.join(rootDir, 'src/index.html'), path.join(distDir, 'index.html'))

// Two entry points: the app, and the PoW solver, which runs as a classic Worker
// and therefore needs its own bundle at a stable URL. `entryNames: '[name]'`
// flattens src/lib/pow.worker.ts to dist/pow.worker.js.
const ctx = await esbuild.context({
	entryPoints: ['src/app.tsx', 'src/lib/pow.worker.ts'],
	bundle: true,
	minify: isProd,
	format: 'iife',
	outdir: 'dist',
	entryNames: '[name]',
	sourcemap: !isProd,
	target: ['es2021'],
	define: {
		'process.env.NODE_ENV': JSON.stringify(isProd ? 'production' : 'development')
	},
	alias: {
		'~': path.join(rootDir, 'src')
	},
	plugins: [stylePlugin({ postcss: { plugins: [tailwindcss] } })],
	loader: { '.svg': 'dataurl', '.png': 'dataurl' },
	resolveExtensions: ['.tsx', '.ts', '.jsx', '.js', '.css'],
	logLevel: 'info'
})

if (process.argv[2] === 'watch') {
	await ctx.watch()
	console.log('Watching for changes...')
} else {
	await ctx.rebuild()
	await ctx.dispose()
	console.log('Build complete')
}
