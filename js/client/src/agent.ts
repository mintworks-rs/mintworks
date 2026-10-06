// SPDX-License-Identifier: MPL-2.0
// An agent run's event stream. `fetch` + `ReadableStream`, not `EventSource`: `EventSource` can
// neither send `Last-Event-ID` on the first connect nor take the refresh-on-401 path.

import { api, refreshAfter401, ServerError } from './http'
import type { AgentRunEvent, ErrorBody } from './types'

export interface StreamRunOptions {
	/** Resume after this seq, e.g. the value a previous `streamRun` resolved to. */
	lastEventId?: number
	signal?: AbortSignal
	/** Reconnects after a dropped connection, each resuming from the last seq seen. Default 3. */
	retries?: number
}

const runPath = (uid: string) => `/api/agent/runs/${encodeURIComponent(uid)}`

async function open(url: string, last: number, signal?: AbortSignal): Promise<Response> {
	const send = () =>
		fetch(url, {
			credentials: 'same-origin',
			headers: { Accept: 'text/event-stream', 'Last-Event-ID': String(last) },
			signal
		})
	let res = await send()
	if (res.status === 401 && (await refreshAfter401())) res = await send()
	if (!res.ok || !res.body) {
		const text = await res.text().catch(() => '')
		let e: ErrorBody | undefined
		try {
			e = JSON.parse(text) as ErrorBody
		} catch {
			e = undefined
		}
		throw new ServerError(
			e?.error?.errCode ?? `E-HTTP-${res.status}`,
			e?.error?.errStr ?? res.statusText,
			res.status
		)
	}
	return res
}

/** Parses SSE frames off `body`, calling `onFrame` per dispatched event. Comments (the server's
 *  keep-alives) and frames without an `id` are skipped. */
async function readFrames(
	body: ReadableStream<BufferSource>,
	onFrame: (id: number, event: string, data: string) => boolean
): Promise<boolean> {
	const reader = body.pipeThrough(new TextDecoderStream()).getReader()
	let buf = ''
	let id = ''
	let event = 'message'
	let data: string[] = []
	for (;;) {
		const { value, done } = await reader.read()
		if (done) return false
		buf += value
		let nl = buf.search(/\r\n|\r|\n/)
		while (nl >= 0) {
			const line = buf.slice(0, nl)
			buf = buf.slice(nl + (buf.startsWith('\r\n', nl) ? 2 : 1))
			nl = buf.search(/\r\n|\r|\n/)
			if (line === '') {
				if (id && data.length > 0 && onFrame(Number(id), event, data.join('\n'))) {
					await reader.cancel()
					return true
				}
				id = ''
				event = 'message'
				data = []
				continue
			}
			if (line.startsWith(':')) continue
			const colon = line.indexOf(':')
			const field = colon < 0 ? line : line.slice(0, colon)
			const val = colon < 0 ? '' : line.slice(colon + 1).replace(/^ /, '')
			if (field === 'id') id = val
			else if (field === 'event') event = val
			else if (field === 'data') data.push(val)
		}
	}
}

/** Streams a run's events to `onEvent` until its final `done`/`error`, the server closing the
 *  stream, or `signal` aborting. Resolves to the last seq seen — pass it back as `lastEventId` to
 *  resume. An abort rejects with the `AbortError`, as `fetch` does. */
export async function streamRun(
	uid: string,
	onEvent: (ev: AgentRunEvent) => void,
	opts: StreamRunOptions = {}
): Promise<number> {
	let last = opts.lastEventId ?? 0
	let retries = opts.retries ?? 3
	for (;;) {
		try {
			const res = await open(`${runPath(uid)}/events`, last, opts.signal)
			const body = res.body as ReadableStream<BufferSource>
			await readFrames(body, (seq, kind, data) => {
				last = seq
				onEvent({ seq, kind, data: JSON.parse(data) } as AgentRunEvent)
				return kind === 'done' || kind === 'error'
			})
			// Final event seen, or the server ended a stream for a run no longer live.
			return last
		} catch (e) {
			// A server answer or an abort is final; only a dropped connection is retried.
			if (e instanceof ServerError || opts.signal?.aborted || retries-- <= 0) throw e
		}
	}
}

/** `POST /api/agent/runs/{uid}/cancel`. A run already over is a no-op. */
export async function cancelRun(uid: string): Promise<void> {
	await api.post<null>(`${runPath(uid)}/cancel`)
}

// vim: ts=4
