// SPDX-License-Identifier: MPL-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { ServerError } from '../src/http'

const json = (body: unknown, status = 200) =>
	new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })

let calls: string[]
// Per-URL responses; an unlisted URL answers 200 `{ ok: true }`.
let routes: Record<string, () => Response>

async function freshHttp() {
	vi.resetModules()
	return import('../src/http')
}

const refreshCalls = () => calls.filter((u) => u === '/api/auth/refresh').length

beforeEach(() => {
	vi.useFakeTimers()
	vi.setSystemTime(0)
	calls = []
	routes = { '/api/auth/refresh': () => json({ expiresIn: 900 }) }
	vi.stubGlobal(
		'fetch',
		vi.fn(async (url: string) => {
			calls.push(url)
			return routes[url]?.() ?? json({ ok: true })
		})
	)
})

afterEach(() => {
	vi.useRealTimers()
	vi.unstubAllGlobals()
})

describe('preventive renewal before a mutation', () => {
	it('renews first while freshness is unknown, then not until the margin, then again', async () => {
		const { api } = await freshHttp()
		await expect(api.post('/api/x')).resolves.toEqual({ ok: true })
		expect(calls).toEqual(['/api/auth/refresh', '/api/x'])

		calls = []
		vi.setSystemTime(13 * 60_000)
		await api.post('/api/x')
		expect(calls).toEqual(['/api/x'])

		calls = []
		vi.setSystemTime(14.5 * 60_000)
		await api.post('/api/x')
		expect(calls).toEqual(['/api/auth/refresh', '/api/x'])
	})

	it('leaves GETs and public auth POSTs alone, even after a failed refresh', async () => {
		const { api } = await freshHttp()
		routes['/api/auth/refresh'] = () => json({}, 503)
		await api.get('/api/x')
		await api.post('/api/y')
		await api.post('/api/auth/login')
		await api.post('/api/auth/register')
		expect(calls).toEqual([
			'/api/x',
			'/api/auth/refresh',
			'/api/y',
			'/api/auth/login',
			'/api/auth/register'
		])
	})

	it('does not sign out on a rejected preflight, and still sends the request', async () => {
		const { api, setOnAuthLost } = await freshHttp()
		const lost = vi.fn()
		setOnAuthLost(lost)
		routes['/api/auth/refresh'] = () => json({}, 401)
		await api.post('/api/x')
		expect(calls).toEqual(['/api/auth/refresh', '/api/x'])
		expect(lost).not.toHaveBeenCalled()
	})

	it('backs off after a rejected refresh, then renews again', async () => {
		const { api } = await freshHttp()
		routes['/api/auth/refresh'] = () => json({}, 401)
		await api.post('/api/x')
		await api.post('/api/x')
		await api.post('/api/x')
		expect(refreshCalls()).toBe(1)
		vi.setSystemTime(5 * 60_000)
		await api.post('/api/x')
		expect(refreshCalls()).toBe(2)
	})

	it('backs off after a logout, then renews again', async () => {
		const { api } = await freshHttp()
		await api.post('/api/x')
		await api.post('/api/auth/logout')
		calls = []
		await api.post('/api/x')
		expect(calls).toEqual(['/api/x'])
		vi.setSystemTime(5 * 60_000)
		calls = []
		await api.post('/api/x')
		expect(calls).toEqual(['/api/auth/refresh', '/api/x'])
	})

	it('refreshes a dead session once, then signs out', async () => {
		const { api, setOnAuthLost } = await freshHttp()
		const lost = vi.fn()
		setOnAuthLost(lost)
		routes['/api/auth/refresh'] = () => json({}, 401)
		routes['/api/x'] = () => json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		await expect(api.post('/api/x')).rejects.toMatchObject({ httpStatus: 401 })
		expect(refreshCalls()).toBe(1)
		expect(lost).toHaveBeenCalledOnce()
	})

	it('asks for a retry when a mutation 401s after a good preflight', async () => {
		const { api } = await freshHttp()
		routes['/api/x'] = () => json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		const e = (await api.post('/api/x').catch((x: unknown) => x)) as ServerError
		expect(e.errCode).toBe('E-AUTH-RETRY')
		expect(refreshCalls()).toBe(1)
	})

	it('shares one preflight between concurrent mutations', async () => {
		const { api } = await freshHttp()
		await Promise.all([api.post('/api/a'), api.post('/api/b')])
		expect(refreshCalls()).toBe(1)
	})

	it('ignores expiresIn from a non-auth route', async () => {
		const { api } = await freshHttp()
		routes['/api/auth/refresh'] = () => json({}, 503)
		routes['/api/x'] = () => json({ expiresIn: 9999 })
		await api.post('/api/x')
		calls = []
		await api.post('/api/x')
		expect(calls).toEqual(['/api/auth/refresh', '/api/x'])
	})

	it('renews ahead of session-bound auth routes', async () => {
		const { api } = await freshHttp()
		await api.post('/api/auth/totp')
		expect(calls).toEqual(['/api/auth/refresh', '/api/auth/totp'])
	})

	it('honours an already-aborted signal', async () => {
		const { api } = await freshHttp()
		const ac = new AbortController()
		ac.abort()
		await expect(api.post('/api/x', {}, ac.signal)).rejects.toMatchObject({ name: 'AbortError' })
		expect(calls).not.toContain('/api/x')
	})
})

describe('the 401 path', () => {
	it('refreshes and retries a GET', async () => {
		const { api } = await freshHttp()
		let first = true
		routes['/api/x'] = () => {
			if (!first) return json({ ok: true })
			first = false
			return json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		}
		await expect(api.get('/api/x')).resolves.toEqual({ ok: true })
		expect(calls).toEqual(['/api/x', '/api/auth/refresh', '/api/x'])
	})

	it('signs out when the refresh is refused', async () => {
		const { api, setOnAuthLost } = await freshHttp()
		const lost = vi.fn()
		setOnAuthLost(lost)
		routes['/api/x'] = () => json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		routes['/api/auth/refresh'] = () => json({}, 401)
		await expect(api.get('/api/x')).rejects.toMatchObject({ httpStatus: 401 })
		expect(lost).toHaveBeenCalledOnce()
	})

	it('reports an unreachable refresh as unavailable', async () => {
		const { api } = await freshHttp()
		routes['/api/x'] = () => json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		routes['/api/auth/refresh'] = () => json({}, 503)
		await expect(api.get('/api/x')).rejects.toMatchObject({ errCode: 'E-CORE-UNAVAILABLE' })
	})

	it('counts a refresh with an unreadable body as ok, with a fallback lifetime', async () => {
		const { api } = await freshHttp()
		let first = true
		routes['/api/x'] = () => {
			if (!first) return json({ ok: true })
			first = false
			return json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		}
		routes['/api/auth/refresh'] = () =>
			new Response(new ReadableStream({ start: (c) => c.error(new Error('reset')) }))
		await expect(api.get('/api/x')).resolves.toEqual({ ok: true })
		calls = []
		vi.setSystemTime(60_000)
		await api.post('/api/x')
		expect(calls).toEqual(['/api/x'])
	})
})

describe('a refresh with no cookie answers 204', () => {
	const noSession = () => new Response(null, { status: 204 })

	it('backs off: one refresh for three anonymous mutations, then renews again', async () => {
		const { api } = await freshHttp()
		routes['/api/auth/refresh'] = noSession
		await api.post('/api/x')
		await api.post('/api/x')
		await api.post('/api/x')
		expect(refreshCalls()).toBe(1)
		vi.setSystemTime(5 * 60_000)
		await api.post('/api/x')
		expect(refreshCalls()).toBe(2)
	})

	it('signs out after a GET 401', async () => {
		const { api, setOnAuthLost } = await freshHttp()
		const lost = vi.fn()
		setOnAuthLost(lost)
		routes['/api/auth/refresh'] = noSession
		routes['/api/x'] = () => json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		await expect(api.get('/api/x')).rejects.toMatchObject({ httpStatus: 401 })
		expect(lost).toHaveBeenCalledOnce()
	})

	it('signs out after a blob 401', async () => {
		const { api, setOnAuthLost } = await freshHttp()
		const lost = vi.fn()
		setOnAuthLost(lost)
		routes['/api/auth/refresh'] = noSession
		routes['/api/pdf'] = () => json({ error: { errCode: 'E-AUTH-TOKEN' } }, 401)
		await expect(api.blob('/api/pdf')).rejects.toMatchObject({ httpStatus: 401 })
		expect(lost).toHaveBeenCalledOnce()
	})

	it('probeSession skips /me only on a 204', async () => {
		const answers: [() => Response, boolean][] = [
			[noSession, false],
			[() => json({ expiresIn: 900 }), true],
			[() => json({}, 401), true],
			[() => json({}, 503), true]
		]
		for (const [answer, ask] of answers) {
			const { probeSession } = await freshHttp()
			routes['/api/auth/refresh'] = answer
			expect(await probeSession()).toBe(ask)
		}
	})
})

// vim: ts=4
