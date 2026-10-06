// SPDX-License-Identifier: MIT-0
import type * as React from 'react'
import { useState } from 'react'
import { Link } from 'react-router-dom'

import { useParties } from '@mintworks/client'
import type { ProjectObject, ProjectStatus } from '~/api/hooks'
import { useDeleteProject, useProjects, useSaveProject } from '~/api/hooks'
import { ConfirmDialog } from '~/components/ConfirmDialog'
import type { Column } from '~/components/DataTable'
import { DataTable } from '~/components/DataTable'
import { Modal } from '~/components/Modal'
import { useToast } from '~/components/Toast'
import {
	Button,
	ErrorBanner,
	Field,
	Input,
	Select,
	SkeletonTable,
	StatusChip
} from '~/components/ui'
import { useT } from '~/i18n'

export const PROJECT_STATUSES: ProjectStatus[] = ['OPEN', 'CLOSED']

export function Projects() {
	const { t, err } = useT()
	const toast = useToast()
	const list = useProjects()
	const parties = useParties()
	const del = useDeleteProject()
	const [open, setOpen] = useState(false)
	const [editing, setEditing] = useState<ProjectObject | null>(null)
	const [deleting, setDeleting] = useState<ProjectObject | null>(null)

	const partyName = (uid: string) =>
		parties.data?.items.find((p) => p.uid === uid)?.name ?? t('common.none')

	function edit(p: ProjectObject | null) {
		setEditing(p)
		setOpen(true)
	}

	const columns: Column<ProjectObject>[] = [
		{
			key: 'name',
			header: t('common.name'),
			cell: (p) => (
				<Link to={`/projects/${p.uid}`} className="font-medium text-accent underline">
					{p.body.name}
				</Link>
			)
		},
		{ key: 'party', header: t('projects.party'), cell: (p) => partyName(p.body.partyUid) },
		{
			key: 'status',
			header: t('projects.status'),
			cell: (p) => <ProjectStatusChip status={p.body.status} />
		},
		{ key: 'notes', header: t('projects.notes'), cell: (p) => p.body.notes ?? '' },
		{
			key: 'actions',
			header: t('common.actions'),
			cell: (p) => (
				<span className="flex justify-end gap-1">
					<Button variant="ghost" onClick={() => edit(p)}>
						{t('common.edit')}
					</Button>
					<Button variant="ghost" onClick={() => setDeleting(p)}>
						{t('common.remove')}
					</Button>
				</span>
			),
			numeric: true
		}
	]

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-start justify-between gap-4">
				<div>
					<h1 className="text-lg font-semibold text-fg">{t('projects.title')}</h1>
					<p className="mt-1 max-w-2xl text-sm text-fg-muted">{t('projects.intro')}</p>
				</div>
				<Button onClick={() => edit(null)}>{t('projects.new')}</Button>
			</div>

			{list.isPending ? (
				<SkeletonTable />
			) : list.error ? (
				<ErrorBanner message={err(list.error)} />
			) : (
				<DataTable
					columns={columns}
					rows={list.data?.items ?? []}
					rowKey={(p) => p.uid}
					caption={t('projects.title')}
					empty={{
						title: t('projects.empty'),
						description: t('projects.empty.body'),
						action: <Button onClick={() => edit(null)}>{t('projects.new')}</Button>
					}}
				/>
			)}

			<Modal
				open={open}
				onClose={() => setOpen(false)}
				title={editing ? t('projects.edit') : t('projects.new')}
				className="w-[min(36rem,calc(100vw-2rem))]"
			>
				{open && (
					<ProjectForm
						key={editing?.uid ?? 'new'}
						project={editing}
						onSaved={() => {
							setOpen(false)
							toast.success(t('projects.saved'))
						}}
						onCancel={() => setOpen(false)}
					/>
				)}
			</Modal>

			<ConfirmDialog
				open={deleting !== null}
				title={t('projects.delete.title')}
				// Says what it does *not* do: the `invoice.ext` rows keyed by each invoice
				// survive, and an invoice outliving its project is not a dangling reference.
				description={t('projects.delete.body', { name: deleting?.body.name ?? '' })}
				confirmLabel={t('common.remove')}
				loading={del.isPending}
				onClose={() => setDeleting(null)}
				onConfirm={() => {
					const uid = deleting?.uid
					if (uid === undefined) return
					void del
						.mutateAsync(uid)
						.then(() => toast.success(t('projects.deleted')))
						.catch((e) => toast.error(err(e)))
						.finally(() => setDeleting(null))
				}}
			/>
		</div>
	)
}

export function ProjectStatusChip({ status }: { status: ProjectStatus }) {
	const { t } = useT()
	return (
		<StatusChip tone={status === 'OPEN' ? 'accent' : 'neutral'}>
			{t(`projects.status.${status}`)}
		</StatusChip>
	)
}

function ProjectForm({
	project,
	onSaved,
	onCancel
}: {
	project: ProjectObject | null
	onSaved: () => void
	onCancel: () => void
}) {
	const { t, err } = useT()
	const parties = useParties()
	const save = useSaveProject(project?.uid ?? null)
	const [name, setName] = useState(project?.body.name ?? '')
	const [partyUid, setPartyUid] = useState(project?.body.partyUid ?? '')
	const [status, setStatus] = useState<ProjectStatus>(project?.body.status ?? 'OPEN')
	const [notes, setNotes] = useState(project?.body.notes ?? '')
	const [touched, setTouched] = useState(false)
	const [error, setError] = useState<string | null>(null)

	const incomplete = name.trim() === '' || partyUid === ''

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		try {
			await save.mutateAsync({ name: name.trim(), partyUid, status, notes: notes.trim() })
			onSaved()
		} catch (e2) {
			setError(err(e2))
		}
	}

	return (
		<form onSubmit={submit} className="space-y-4">
			<ErrorBanner message={error} />

			<Field
				label={t('common.name')}
				htmlFor="project-name"
				required
				error={touched && name.trim() === '' ? t('common.required') : undefined}
			>
				<Input
					value={name}
					onBlur={() => setTouched(true)}
					onChange={(e) => setName(e.target.value)}
				/>
			</Field>

			<div className="grid gap-4 sm:grid-cols-2">
				<Field label={t('projects.party')} htmlFor="project-party" required>
					<Select value={partyUid} onChange={(e) => setPartyUid(e.target.value)}>
						<option value="">{t('common.choose')}</option>
						{(parties.data?.items ?? []).map((p) => (
							<option key={p.uid} value={p.uid}>
								{p.name}
							</option>
						))}
					</Select>
				</Field>
				<Field label={t('projects.status')} htmlFor="project-status" required>
					<Select
						value={status}
						onChange={(e) => setStatus(e.target.value as ProjectStatus)}
					>
						{PROJECT_STATUSES.map((s) => (
							<option key={s} value={s}>
								{t(`projects.status.${s}`)}
							</option>
						))}
					</Select>
				</Field>
			</div>

			<Field label={t('projects.notes')} htmlFor="project-notes">
				<Input value={notes} onChange={(e) => setNotes(e.target.value)} />
			</Field>

			<div className="flex justify-end gap-2 pt-2">
				<Button variant="secondary" type="button" onClick={onCancel}>
					{t('common.cancel')}
				</Button>
				<Button type="submit" loading={save.isPending} disabled={incomplete}>
					{t('common.save')}
				</Button>
			</div>
		</form>
	)
}

// vim: ts=4
