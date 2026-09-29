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

// Wiped first, or a dev build's full-source app.js.map is served beside a production app.js.
// index.html is copied once per invocation, so `watch` misses edits to it: restart the watcher.
rmSync(distDir, { recursive: true, force: true })
await mkdir(distDir, { recursive: true })
await cp(path.join(rootDir, 'src/index.html'), path.join(distDir, 'index.html'))

// Two entry points: the app, and the PoW solver, which runs as a classic Worker
// and therefore needs its own bundle at the stable URL `POW_WORKER_URL` names.
const ctx = await esbuild.context({
	entryPoints: [
		{ in: 'src/app.tsx', out: 'app' },
		{ in: '../../../js/saas-client/src/pow/worker.ts', out: 'pow.worker' }
	],
	bundle: true,
	minify: isProd,
	format: 'iife',
	outdir: 'dist',
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
