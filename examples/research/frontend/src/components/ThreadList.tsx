// SPDX-License-Identifier: MIT-0
import { useState } from 'react'
import { Link } from 'react-router-dom'

import { ERRORS_EN, errText } from '@mintworks/client'
import { useCreateProject, useProjects, useThreads } from '~/api/hooks'
import { Button, buttonClass, ErrorBanner, Input, Spinner } from '~/components/ui'

const NEW = '__new'

function ProjectPicker({
	project,
	onProject
}: {
	project: string | null
	onProject: (p: string | null) => void
}) {
	const projects = useProjects()
	const create = useCreateProject()
	const [naming, setNaming] = useState(false)
	const [name, setName] = useState('')

	if (naming)
		return (
			<form
				className="flex flex-col gap-2"
				onSubmit={(e) => {
					e.preventDefault()
					if (!name.trim()) return
					create.mutate(name.trim(), {
						onSuccess: (p) => {
							onProject(p.uid)
							setNaming(false)
							setName('')
						}
					})
				}}
			>
				<Input
					aria-label="Project name"
					placeholder="Project name"
					value={name}
					onChange={(e) => setName(e.target.value)}
					autoFocus
				/>
				<div className="flex gap-2">
					<Button type="submit" loading={create.isPending}>
						Create
					</Button>
					<Button type="button" variant="ghost" onClick={() => setNaming(false)}>
						Cancel
					</Button>
				</div>
				<ErrorBanner message={create.error ? errText(create.error, ERRORS_EN) : null} />
			</form>
		)
	return (
		<select
			aria-label="Project"
			value={project ?? ''}
			onChange={(e) => {
				if (e.target.value === NEW) setNaming(true)
				else onProject(e.target.value || null)
			}}
			className="min-h-[44px] rounded-md border border-line-strong bg-surface-raised px-2 text-sm"
		>
			<option value="">No project</option>
			{projects.data?.map((p) => (
				<option key={p.uid} value={p.uid}>
					{p.name}
				</option>
			))}
			<option value={NEW}>New project…</option>
		</select>
	)
}

export function ThreadList({
	active,
	project,
	onProject,
	onNavigate
}: {
	active?: string
	project: string | null
	onProject: (p: string | null) => void
	onNavigate: () => void
}) {
	const { data, isPending, error } = useThreads()
	const shown = data?.filter((t) => t.project === project)
	return (
		<nav className="flex h-full flex-col gap-3 p-3" aria-label="Threads">
			<ProjectPicker project={project} onProject={onProject} />
			<Link to="/" onClick={onNavigate} className={buttonClass('primary')}>
				New research
			</Link>
			<ErrorBanner message={error ? error.message : null} />
			{isPending && <Spinner className="mx-auto" />}
			<ul className="-mx-1 flex-1 space-y-0.5 overflow-y-auto">
				{shown?.map((t) => (
					<li key={t.uid}>
						<Link
							to={`/t/${t.uid}`}
							onClick={onNavigate}
							aria-current={t.uid === active ? 'page' : undefined}
							className={
								'block truncate rounded-md px-2 py-2 text-sm hover:bg-surface-sunken ' +
								(t.uid === active
									? 'bg-surface-sunken font-medium'
									: 'text-fg-muted')
							}
						>
							{t.title || 'Untitled'}
						</Link>
					</li>
				))}
				{shown?.length === 0 && (
					<li className="px-2 text-sm text-fg-muted">No research yet.</li>
				)}
			</ul>
		</nav>
	)
}

// vim: ts=4
