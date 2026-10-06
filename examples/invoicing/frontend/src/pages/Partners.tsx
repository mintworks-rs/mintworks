// SPDX-License-Identifier: MIT-0
import type * as React from 'react'
import { useState } from 'react'

import type { BillingParty, PartyKind } from '@mintworks/client'
import { useDeleteParty, useParties, useSaveParty, useSeller } from '@mintworks/client'
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

interface Form {
	kind: PartyKind
	name: string
	country: string
	taxNumber: string
	euVatId: string
	groupTaxNo: string
	postcode: string
	city: string
	street: string
	email: string
	isDefault: boolean
	paymentDays: string
	/** `''` is the company default. */
	paymentMethod: '' | 'TRANSFER' | 'CASH'
}

const EMPTY: Form = {
	kind: 'C',
	name: '',
	country: 'HU',
	taxNumber: '',
	euVatId: '',
	groupTaxNo: '',
	postcode: '',
	city: '',
	street: '',
	email: '',
	isDefault: false,
	paymentDays: '',
	paymentMethod: ''
}

/** Every nullable field is a three-state `Patch` server-side, so a field the user blanked out
 *  has to travel as `null` — leaving it out means "keep what is there". */
const orNull = (s: string) => (s.trim() === '' ? null : s.trim())

export function Partners() {
	const { t, err } = useT()
	const toast = useToast()
	const list = useParties()
	const del = useDeleteParty()
	const [open, setOpen] = useState(false)
	const [editing, setEditing] = useState<BillingParty | null>(null)
	const [deleting, setDeleting] = useState<BillingParty | null>(null)

	function edit(p: BillingParty | null) {
		setEditing(p)
		setOpen(true)
	}

	const columns: Column<BillingParty>[] = [
		{
			key: 'name',
			header: t('common.name'),
			cell: (p) => (
				<span className="flex items-center gap-2">
					<span className="font-medium">{p.name}</span>
					{p.paymentDays != null && (
						<span className="text-fg-muted">
							{t('partners.days', { n: String(p.paymentDays) })}
						</span>
					)}
					{p.isDefault && <StatusChip tone="accent">{t('partners.default')}</StatusChip>}
				</span>
			)
		},
		{ key: 'kind', header: t('partners.kind'), cell: (p) => t(`partners.kind.${p.kind}`) },
		{
			key: 'tax',
			header: t('partners.taxNumber'),
			cell: (p) => p.taxNumber ?? t('common.none')
		},
		{
			key: 'city',
			header: t('partners.city'),
			cell: (p) => [p.country, p.city].filter(Boolean).join(' · ')
		},
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
					<h1 className="text-lg font-semibold text-fg">{t('partners.title')}</h1>
					<p className="mt-1 max-w-2xl text-sm text-fg-muted">{t('partners.intro')}</p>
				</div>
				<Button onClick={() => edit(null)}>{t('partners.new')}</Button>
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
					caption={t('partners.title')}
					empty={{
						title: t('partners.empty'),
						description: t('partners.empty.body'),
						action: <Button onClick={() => edit(null)}>{t('partners.new')}</Button>
					}}
				/>
			)}

			<Modal
				open={open}
				onClose={() => setOpen(false)}
				title={editing ? t('partners.edit') : t('partners.new')}
				className="w-[min(40rem,calc(100vw-2rem))]"
			>
				{open && (
					<PartyForm
						key={editing?.uid ?? 'new'}
						party={editing}
						onSaved={() => {
							setOpen(false)
							toast.success(t('partners.saved'))
						}}
						onCancel={() => setOpen(false)}
					/>
				)}
			</Modal>

			<ConfirmDialog
				open={deleting !== null}
				title={t('partners.delete.title')}
				description={t('partners.delete.body', { name: deleting?.name ?? '' })}
				confirmLabel={t('common.remove')}
				loading={del.isPending}
				onClose={() => setDeleting(null)}
				onConfirm={() => {
					const uid = deleting?.uid
					if (uid === undefined) return
					void del
						.mutateAsync(uid)
						.then(() => toast.success(t('partners.deleted')))
						.catch((e) => toast.error(err(e)))
						.finally(() => setDeleting(null))
				}}
			/>
		</div>
	)
}

function PartyForm({
	party,
	onSaved,
	onCancel
}: {
	party: BillingParty | null
	onSaved: () => void
	onCancel: () => void
}) {
	const { t, err } = useT()
	const save = useSaveParty(party?.uid ?? null)
	const seller = useSeller()
	const [f, setF] = useState<Form>(
		party === null
			? EMPTY
			: {
					kind: party.kind,
					name: party.name,
					country: party.country,
					taxNumber: party.taxNumber ?? '',
					euVatId: party.euVatId ?? '',
					groupTaxNo: party.groupTaxNo ?? '',
					postcode: party.postcode ?? '',
					city: party.city ?? '',
					street: party.street ?? '',
					email: party.email ?? '',
					isDefault: party.isDefault,
					paymentDays: party.paymentDays?.toString() ?? '',
					paymentMethod: party.paymentMethod ?? ''
				}
	)
	const [touched, setTouched] = useState<Record<string, boolean>>({})
	const [error, setError] = useState<string | null>(null)

	const set = (k: keyof Form, v: string | boolean) => setF((p) => ({ ...p, [k]: v }) as Form)
	// On blur, not while typing: a required-field message that appears at the first keystroke
	// scolds someone who is still answering.
	const blur = (k: keyof Form) => () => setTouched((p) => ({ ...p, [k]: true }))
	const missing = (k: 'name' | 'country') =>
		touched[k] && f[k].trim() === '' ? t('common.required') : undefined

	const incomplete = f.name.trim() === '' || f.country.trim() === ''

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		try {
			await save.mutateAsync({
				kind: f.kind,
				name: f.name.trim(),
				country: f.country.trim().toUpperCase(),
				taxNumber: orNull(f.taxNumber),
				euVatId: orNull(f.euVatId),
				groupTaxNo: orNull(f.groupTaxNo),
				postcode: orNull(f.postcode),
				city: orNull(f.city),
				street: orNull(f.street),
				email: orNull(f.email),
				isDefault: f.isDefault,
				paymentDays: f.paymentDays.trim() === '' ? null : Number(f.paymentDays),
				paymentMethod: f.paymentMethod === '' ? null : f.paymentMethod
			})
			onSaved()
		} catch (e2) {
			setError(err(e2))
		}
	}

	return (
		<form onSubmit={submit} className="space-y-4">
			<ErrorBanner message={error} />

			<div className="grid gap-4 sm:grid-cols-2">
				<Field label={t('partners.kind')} htmlFor="party-kind" required>
					<Select value={f.kind} onChange={(e) => set('kind', e.target.value)}>
						<option value="C">{t('partners.kind.C')}</option>
						<option value="P">{t('partners.kind.P')}</option>
					</Select>
				</Field>
				<Field
					label={t('partners.country')}
					htmlFor="party-country"
					required
					hint={t('partners.country.hint')}
					error={missing('country')}
				>
					<Input
						maxLength={2}
						autoCapitalize="characters"
						value={f.country}
						onBlur={blur('country')}
						onChange={(e) => set('country', e.target.value)}
					/>
				</Field>
			</div>

			<Field label={t('common.name')} htmlFor="party-name" required error={missing('name')}>
				<Input
					value={f.name}
					onBlur={blur('name')}
					onChange={(e) => set('name', e.target.value)}
				/>
			</Field>

			<div className="grid gap-4 sm:grid-cols-3">
				<Field label={t('partners.taxNumber')} htmlFor="party-tax">
					<Input value={f.taxNumber} onChange={(e) => set('taxNumber', e.target.value)} />
				</Field>
				<Field label={t('partners.euVatId')} htmlFor="party-euvat">
					<Input value={f.euVatId} onChange={(e) => set('euVatId', e.target.value)} />
				</Field>
				<Field label={t('partners.groupTaxNo')} htmlFor="party-group">
					<Input
						value={f.groupTaxNo}
						onChange={(e) => set('groupTaxNo', e.target.value)}
					/>
				</Field>
			</div>

			<div className="grid gap-4 sm:grid-cols-3">
				<Field label={t('partners.postcode')} htmlFor="party-postcode">
					<Input value={f.postcode} onChange={(e) => set('postcode', e.target.value)} />
				</Field>
				<Field label={t('partners.city')} htmlFor="party-city">
					<Input value={f.city} onChange={(e) => set('city', e.target.value)} />
				</Field>
				<Field label={t('partners.street')} htmlFor="party-street">
					<Input value={f.street} onChange={(e) => set('street', e.target.value)} />
				</Field>
			</div>

			<Field label={t('common.email')} htmlFor="party-email">
				<Input
					type="email"
					value={f.email}
					onChange={(e) => set('email', e.target.value)}
				/>
			</Field>

			<div className="grid gap-4 sm:grid-cols-2">
				<Field label={t('partners.paymentDays')} htmlFor="party-days">
					<Input
						type="number"
						min={0}
						max={36500}
						placeholder={
							seller.data
								? t('partners.companyDefault', {
										n: String(
											seller.data.paymentDays ??
												seller.data.defaultPaymentDays
										)
									})
								: undefined
						}
						value={f.paymentDays}
						onChange={(e) => set('paymentDays', e.target.value)}
					/>
				</Field>
				<Field label={t('partners.paymentMethod')} htmlFor="party-method">
					<Select
						value={f.paymentMethod}
						onChange={(e) => set('paymentMethod', e.target.value)}
					>
						<option value="">{t('partners.methodDefault')}</option>
						<option value="TRANSFER">{t('invoices.pay.TRANSFER')}</option>
						<option value="CASH">{t('invoices.pay.CASH')}</option>
					</Select>
				</Field>
			</div>

			<label className="flex items-center gap-2 text-sm text-fg">
				<input
					type="checkbox"
					checked={f.isDefault}
					onChange={(e) => set('isDefault', e.target.checked)}
					className="h-4 w-4 accent-accent"
				/>
				{t('partners.isDefault')}
			</label>

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
