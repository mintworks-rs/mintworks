// SPDX-License-Identifier: MIT-0
import { Spinner } from '~/components/ui'

export interface Step {
	id: string
	name: string
	arguments: string
	state: 'running' | 'ok' | 'failed'
}

/** The one argument worth showing: a search query, a URL or a notebook path. */
function gist(args: string): string {
	try {
		const a = JSON.parse(args) as Record<string, unknown>
		const v = a.query ?? a.url ?? a.path ?? a.name
		if (typeof v === 'string') return v
	} catch {
		// Raw string below.
	}
	return args.length > 80 ? `${args.slice(0, 80)}…` : args
}

/** Tool calls as one collapsed line; expands to the call list. */
export function Steps({ steps }: { steps: Step[] }) {
	if (steps.length === 0) return null
	const running = steps.some((s) => s.state === 'running')
	const last = steps[steps.length - 1]
	return (
		<details className="text-xs text-fg-muted">
			<summary className="flex cursor-pointer list-none items-center gap-2 select-none">
				{running ? <Spinner className="h-3 w-3" /> : <span aria-hidden="true">▸</span>}
				<span>
					{steps.length} {steps.length === 1 ? 'step' : 'steps'}
					{running && ` — ${last.name}`}
				</span>
			</summary>
			<ul className="mt-1 space-y-0.5 pl-5">
				{steps.map((s) => (
					<li key={s.id} className="truncate">
						<span className={s.state === 'failed' ? 'text-danger' : ''}>
							{s.state === 'failed' ? '✕' : s.state === 'ok' ? '✓' : '…'} {s.name}
						</span>{' '}
						<span className="opacity-80">{gist(s.arguments)}</span>
					</li>
				))}
			</ul>
		</details>
	)
}

// vim: ts=4
