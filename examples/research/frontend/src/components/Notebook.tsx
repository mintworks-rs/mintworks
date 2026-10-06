import { useState } from 'react'

import { ERRORS_EN, errText } from '@mintworks/client'
import {
	type Space,
	pdfUrl,
	useDocument,
	useExportPdf,
	useNotebook,
	useNotebookDoc,
	useNotebookHistory
} from '~/api/hooks'
import { Markdown } from '~/components/Markdown'
import { Button, buttonClass, ErrorBanner, Spinner } from '~/components/ui'

function Export({ space, name, version }: { space: Space; name: string; version?: number }) {
	const exp = useExportPdf(space)
	const doc = useDocument(exp.data?.uid ?? '')
	const status = doc.data?.status
	return (
		<div className="flex items-center gap-2">
			{status === 'READY' && exp.data ? (
				<a
					href={pdfUrl(exp.data.uid)}
					target="_blank"
					rel="noopener noreferrer"
					className={buttonClass('secondary')}
				>
					Open PDF
				</a>
			) : (
				<Button
					variant="secondary"
					loading={exp.isPending || status === 'PENDING'}
					// The export renders the latest version; an older one on screen would mislead.
					disabled={version !== undefined}
					onClick={() => exp.mutate(name)}
				>
					Export PDF
				</Button>
			)}
			{status === 'FAILED' && <span className="text-xs text-danger">Render failed.</span>}
			{exp.error && (
				<span className="text-xs text-danger">{errText(exp.error, ERRORS_EN)}</span>
			)}
		</div>
	)
}

function Doc({ space, name }: { space: Space; name: string }) {
	const [version, setVersion] = useState<number | undefined>()
	const history = useNotebookHistory(space, name)
	const doc = useNotebookDoc(space, name, version)
	return (
		<div className="space-y-3">
			<div className="flex flex-wrap items-center gap-2">
				<select
					aria-label="Version"
					value={version ?? ''}
					onChange={(e) =>
						setVersion(e.target.value ? Number(e.target.value) : undefined)
					}
					className="min-h-[44px] rounded-md border border-line-strong bg-surface-raised px-2 text-sm"
				>
					<option value="">Latest</option>
					{history.data
						?.slice()
						.reverse()
						.map((v) => (
							<option key={v.version} value={v.version}>
								v{v.version} — {new Date(v.createdAt).toLocaleString()}
							</option>
						))}
				</select>
				<Export key={name} space={space} name={name} version={version} />
			</div>
			{doc.isPending && <Spinner />}
			<ErrorBanner message={doc.error ? errText(doc.error, ERRORS_EN) : null} />
			{doc.data && <Markdown text={doc.data.body} />}
		</div>
	)
}

/** Read-only: the agent writes the notebook, the user browses and exports it. */
export function Notebook({ project }: { project: string | null }) {
	const tabs: [string, Space][] = project
		? [
				['Notes', project],
				['Reports', `reports-${project}`],
				['Global', 'global']
			]
		: [['Global', 'global']]
	const [picked, setPicked] = useState<Space>('global')
	const space = tabs.some(([, s]) => s === picked) ? picked : tabs[0][1]
	return (
		<section className="flex h-full min-h-0 flex-col" aria-label="Notebook">
			{tabs.length > 1 && (
				<div role="tablist" className="flex gap-1 border-b border-line px-3 pt-2">
					{tabs.map(([label, s]) => (
						<button
							key={s}
							type="button"
							role="tab"
							aria-selected={s === space}
							onClick={() => setPicked(s)}
							className={
								'min-h-[44px] rounded-t-md px-3 text-sm ' +
								(s === space ? 'bg-surface-sunken font-medium' : 'text-fg-muted')
							}
						>
							{label}
						</button>
					))}
				</div>
			)}
			<SpaceView key={space} space={space} />
		</section>
	)
}

function SpaceView({ space }: { space: Space }) {
	const list = useNotebook(space)
	const [open, setOpen] = useState<string | null>(null)
	return (
		<>
			<h2 className="flex items-center gap-2 border-b border-line p-3 text-sm font-semibold">
				{open && (
					<button
						type="button"
						onClick={() => setOpen(null)}
						className="rounded px-1 text-fg-muted hover:bg-surface-sunken"
						aria-label="Back to notebook"
					>
						←
					</button>
				)}
				<span className="truncate">{open ?? 'Notebook'}</span>
			</h2>
			<div className="flex-1 overflow-y-auto p-3">
				{open ? (
					<Doc key={open} space={space} name={open} />
				) : (
					<>
						{list.isPending && <Spinner />}
						<ErrorBanner message={list.error ? errText(list.error, ERRORS_EN) : null} />
						<ul className="space-y-0.5">
							{list.data?.map((d) => (
								<li key={d.path}>
									<button
										type="button"
										onClick={() => setOpen(d.path)}
										className="w-full truncate rounded-md px-2 py-2 text-left text-sm hover:bg-surface-sunken"
									>
										{d.path}
										<span className="ml-2 text-xs text-fg-muted">
											v{d.version}
										</span>
									</button>
								</li>
							))}
						</ul>
						{list.data?.length === 0 && (
							<p className="text-sm text-fg-muted">
								Empty. Findings the assistant saves appear here.
							</p>
						)}
					</>
				)}
			</div>
		</>
	)
}

// vim: ts=4
