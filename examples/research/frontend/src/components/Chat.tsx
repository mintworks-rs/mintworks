// SPDX-License-Identifier: MIT-0
import { useQueryClient } from '@tanstack/react-query'
import { useEffect, useRef, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'

import { cancelRun, ERRORS_EN, errText, streamRun } from '@mintworks/client'
import { type ChatMessage, researchKeys, useAsk, useMessages, useStartThread } from '~/api/hooks'
import { Markdown } from '~/components/Markdown'
import { type Step, Steps } from '~/components/Steps'
import { Button, ErrorBanner, Spinner } from '~/components/ui'

/** Handed over by `navigate` when a new thread starts, so its first run streams here. */
interface StartState {
	run: string
	question: string
}

interface Live {
	question: string | null
	/** Finished assistant turns of the run so far, then the one being streamed. */
	parts: string[]
	text: string
	steps: Step[]
}

const idle: Live = { question: null, parts: [], text: '', steps: [] }

function UserBubble({ text }: { text: string }) {
	return (
		<div className="ml-auto max-w-[85%] whitespace-pre-wrap rounded-lg bg-surface-sunken px-3 py-2 text-sm">
			{text}
		</div>
	)
}

/** Stored history: tool-role messages carry no content and are skipped; the calls an assistant
 *  turn made collapse into one finished steps line. */
function History({ messages }: { messages: ChatMessage[] }) {
	return messages.map((m, i) => {
		const key = `${m.createdAt}-${i}`
		if (m.role === 'user') return <UserBubble key={key} text={m.content} />
		if (m.role !== 'assistant') return null
		const steps: Step[] = (m.toolCalls ?? []).map((c, j) => ({
			id: `${key}-${j}`,
			name: c.name,
			arguments: c.arguments,
			state: 'ok'
		}))
		return (
			<div key={key} className="space-y-2">
				<Steps steps={steps} />
				{m.content && <Markdown text={m.content} />}
			</div>
		)
	})
}

export function Chat({ thread, project }: { thread?: string; project: string | null }) {
	const qc = useQueryClient()
	const navigate = useNavigate()
	const start = useLocation().state as StartState | null
	const messages = useMessages(thread ?? '')
	const startThread = useStartThread()
	const ask = useAsk(thread ?? '')

	const [run, setRun] = useState<string | null>(start?.run ?? null)
	const [live, setLive] = useState<Live>({ ...idle, question: start?.question ?? null })
	const [runError, setRunError] = useState<string | null>(null)
	const [input, setInput] = useState('')
	const bottom = useRef<HTMLDivElement>(null)

	useEffect(() => {
		if (!run) return
		const ctl = new AbortController()
		const finish = async () => {
			await Promise.all([
				qc.invalidateQueries({ queryKey: ['threads'] }),
				qc.invalidateQueries({ queryKey: researchKeys.usage })
			])
			setLive(idle)
			setRun(null)
			// Drop the start state, or a reload would stream this finished run again.
			if (start) navigate('.', { replace: true, state: null })
		}
		streamRun(
			run,
			(ev) => {
				switch (ev.kind) {
					case 'delta':
						setLive((l) => ({ ...l, text: l.text + ev.data.text }))
						break
					case 'message':
						setLive((l) => ({
							...l,
							parts: ev.data.content ? [...l.parts, ev.data.content] : l.parts,
							text: ''
						}))
						break
					case 'tool_call':
						setLive((l) => ({
							...l,
							steps: [...l.steps, { ...ev.data, state: 'running' }]
						}))
						break
					case 'tool_result': {
						const { id, ok, name } = ev.data
						setLive((l) => ({
							...l,
							steps: l.steps.map((s) =>
								s.id === id ? { ...s, state: ok ? 'ok' : 'failed' } : s
							)
						}))
						if (ok && (name === 'memory_write' || name === 'memory_append'))
							void qc.invalidateQueries({ queryKey: researchKeys.notebooks })
						break
					}
					case 'error':
						setRunError(
							`${ev.data.errCode ?? 'Run failed'}${ev.data.errStr ? `: ${ev.data.errStr}` : ''}`
						)
						break
				}
			},
			{ signal: ctl.signal }
		)
			.catch((e) => {
				if (!ctl.signal.aborted) setRunError(errText(e, ERRORS_EN))
			})
			.finally(() => {
				if (!ctl.signal.aborted) void finish()
			})
		return () => ctl.abort()
	}, [run, qc, start, navigate])

	// Braces, not an expression body: Chrome's scrollIntoView returns a Promise, which React
	// would take for the cleanup function.
	useEffect(() => {
		bottom.current?.scrollIntoView({ block: 'end' })
	}, [messages.data, live])

	async function send() {
		const text = input.trim()
		if (!text || run) return
		setRunError(null)
		try {
			if (!thread) {
				const r = await startThread.mutateAsync({ question: text, project })
				navigate(`/t/${r.thread}`, { state: { run: r.run, question: text } })
			} else {
				const r = await ask.mutateAsync(text)
				setLive({ ...idle, question: text })
				setRun(r.run)
			}
			setInput('')
		} catch (e) {
			setRunError(errText(e, ERRORS_EN))
		}
	}

	const busy = run !== null || startThread.isPending || ask.isPending
	return (
		<section className="flex h-full min-h-0 flex-col" aria-label="Chat">
			<div className="flex-1 space-y-4 overflow-y-auto p-4">
				{!thread && !busy && (
					<p className="mt-16 text-center text-sm text-fg-muted">
						Ask a question. The assistant searches the web, cites its sources and keeps
						durable findings in the notebook.
					</p>
				)}
				{messages.isPending && thread && <Spinner className="mx-auto block" />}
				{messages.error && <ErrorBanner message={errText(messages.error, ERRORS_EN)} />}
				{messages.data && <History messages={messages.data} />}
				{live.question && <UserBubble text={live.question} />}
				{run && (
					<div className="space-y-2">
						<Steps steps={live.steps} />
						{live.parts.map((p, i) => (
							<Markdown key={i} text={p} />
						))}
						{live.text ? (
							<Markdown text={live.text} />
						) : (
							live.steps.length === 0 && <Spinner />
						)}
					</div>
				)}
				<ErrorBanner message={runError} />
				<div ref={bottom} />
			</div>
			<form
				className="flex items-end gap-2 border-t border-line p-3"
				onSubmit={(e) => {
					e.preventDefault()
					void send()
				}}
			>
				<textarea
					value={input}
					onChange={(e) => setInput(e.target.value)}
					onKeyDown={(e) => {
						if (e.key === 'Enter' && !e.shiftKey) {
							e.preventDefault()
							void send()
						}
					}}
					rows={2}
					placeholder={thread ? 'Ask a follow-up…' : 'What do you want to research?'}
					aria-label="Question"
					className="min-h-[44px] flex-1 resize-none rounded-md border border-line-strong bg-surface-raised px-3 py-2 text-sm text-fg placeholder:text-fg-muted focus:border-accent"
				/>
				{run ? (
					<Button type="button" variant="secondary" onClick={() => void cancelRun(run)}>
						Stop
					</Button>
				) : (
					<Button type="submit" loading={busy} disabled={!input.trim()}>
						Send
					</Button>
				)}
			</form>
		</section>
	)
}

// vim: ts=4
