// SPDX-License-Identifier: MIT-0
import type * as React from 'react'
import { useState } from 'react'

import type { ServiceView } from '@mintworks/client'
import {
	parseAmount,
	useAuth,
	useSaveService,
	useServices,
	useSetServiceActive
} from '@mintworks/client'
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
	MoneyText,
	Select,
	SkeletonTable,
	StatusChip
} from '~/components/ui'
import { useT } from '~/i18n'

/** `crates/invoice/src/vat.rs` — the statutory codes, never translated. */
export const VAT_CODES = ['STD27', 'RED18', 'RED05', 'AAM', 'TAM', 'EUFAD37', 'HO', 'ATK']

interface Form {
	code: string
	name: string
	description: string
	unit: string
	unitPrice: string
	vatCode: string
}

const EMPTY: Form = {
	code: '',
	name: '',
	description: '',
	unit: 'db',
	unitPrice: '',
	vatCode: 'STD27'
}

export function Services() {
	const { t, err } = useT()
	const { me } = useAuth()
	const toast = useToast()
	const [showInactive, setShowInactive] = useState(false)
	const list = useServices(showInactive)
	const setActive = useSetServiceActive()
	const [open, setOpen] = useState(false)
	const [editing, setEditing] = useState<ServiceView | null>(null)
	const [deactivating, setDeactivating] = useState<ServiceView | null>(null)

	// Every `unitPrice` the server sends is quoted in the deployment's `currency.base`, so any
	// row answers what a new price must be in. With no rows at all there is nothing to read it
	// from and the org's billing currency is the closest guess.
	const currency = list.data?.items[0]?.unitPrice.currency ?? me?.org?.billingCurrency ?? 'HUF'

	function edit(s: ServiceView | null) {
		setEditing(s)
		setOpen(true)
	}

	const columns: Column<ServiceView>[] = [
		{ key: 'code', header: t('services.code'), cell: (s) => s.code ?? t('common.none') },
		{
			key: 'name',
			header: t('common.name'),
			cell: (s) => (
				<span className="flex items-center gap-2">
					<span className="font-medium">{s.name}</span>
					{!s.active && <StatusChip tone="neutral">{t('services.inactive')}</StatusChip>}
				</span>
			)
		},
		{ key: 'unit', header: t('services.unit'), cell: (s) => s.unit },
		{
			key: 'price',
			header: t('services.unitPrice'),
			cell: (s) => <MoneyText value={s.unitPrice} />,
			numeric: true
		},
		{ key: 'vat', header: t('services.vatCode'), cell: (s) => s.vatCode },
		{
			key: 'actions',
			header: t('common.actions'),
			cell: (s) => (
				<span className="flex justify-end gap-1">
					<Button variant="ghost" onClick={() => edit(s)}>
						{t('common.edit')}
					</Button>
					{s.active ? (
						<Button variant="ghost" onClick={() => setDeactivating(s)}>
							{t('services.deactivate')}
						</Button>
					) : (
						<Button
							variant="ghost"
							onClick={() =>
								void setActive
									.mutateAsync({ uid: s.uid, active: true })
									.then(() => toast.success(t('services.activated')))
									.catch((e) => toast.error(err(e)))
							}
						>
							{t('services.activate')}
						</Button>
					)}
				</span>
			),
			numeric: true
		}
	]

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-start justify-between gap-4">
				<div>
					<h1 className="text-lg font-semibold text-fg">{t('services.title')}</h1>
					<p className="mt-1 max-w-2xl text-sm text-fg-muted">{t('services.intro')}</p>
				</div>
				<Button onClick={() => edit(null)}>{t('services.new')}</Button>
			</div>

			<label className="flex items-center gap-2 text-sm text-fg-muted">
				<input
					type="checkbox"
					checked={showInactive}
					onChange={(e) => setShowInactive(e.target.checked)}
					className="h-4 w-4 accent-accent"
				/>
				{t('services.showInactive')}
			</label>

			{list.isPending ? (
				<SkeletonTable />
			) : list.error ? (
				<ErrorBanner message={err(list.error)} />
			) : (
				<DataTable
					columns={columns}
					rows={list.data?.items ?? []}
					rowKey={(s) => s.uid}
					caption={t('services.title')}
					empty={{
						title: t('services.empty'),
						description: t('services.empty.body'),
						action: <Button onClick={() => edit(null)}>{t('services.new')}</Button>
					}}
				/>
			)}

			<Modal
				open={open}
				onClose={() => setOpen(false)}
				title={editing ? t('services.edit') : t('services.new')}
				className="w-[min(40rem,calc(100vw-2rem))]"
			>
				{open && (
					<ServiceForm
						key={editing?.uid ?? 'new'}
						service={editing}
						currency={currency}
						onSaved={() => {
							setOpen(false)
							toast.success(t('services.saved'))
						}}
						onCancel={() => setOpen(false)}
					/>
				)}
			</Modal>

			<ConfirmDialog
				open={deactivating !== null}
				title={t('services.deactivate.title')}
				description={t('services.deactivate.body', { name: deactivating?.name ?? '' })}
				confirmLabel={t('services.deactivate')}
				loading={setActive.isPending}
				onClose={() => setDeactivating(null)}
				onConfirm={() => {
					const uid = deactivating?.uid
					if (uid === undefined) return
					void setActive
						.mutateAsync({ uid, active: false })
						.then(() => toast.success(t('services.deactivated')))
						.catch((e) => toast.error(err(e)))
						.finally(() => setDeactivating(null))
				}}
			/>
		</div>
	)
}

function ServiceForm({
	service,
	currency,
	onSaved,
	onCancel
}: {
	service: ServiceView | null
	currency: string
	onSaved: () => void
	onCancel: () => void
}) {
	const { t, err } = useT()
	const save = useSaveService(service?.uid ?? null)
	const [f, setF] = useState<Form>(
		service === null
			? EMPTY
			: {
					code: service.code ?? '',
					name: service.name,
					description: service.description ?? '',
					unit: service.unit,
					unitPrice: service.unitPrice.amount,
					vatCode: service.vatCode
				}
	)
	const [touched, setTouched] = useState<Record<string, boolean>>({})
	const [error, setError] = useState<string | null>(null)

	const set = (k: keyof Form, v: string) => setF((p) => ({ ...p, [k]: v }) as Form)
	const blur = (k: keyof Form) => () => setTouched((p) => ({ ...p, [k]: true }))
	const amount = parseAmount(f.unitPrice, currency)
	const missing = (k: 'code' | 'name' | 'unit') =>
		touched[k] && f[k].trim() === '' ? t('common.required') : undefined
	const priceError =
		touched.unitPrice && amount === null
			? f.unitPrice.trim() === ''
				? t('common.required')
				: t('services.badAmount')
			: undefined

	const incomplete =
		f.code.trim() === '' || f.name.trim() === '' || f.unit.trim() === '' || amount === null

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		if (amount === null) return
		try {
			await save.mutateAsync({
				code: f.code.trim(),
				name: f.name.trim(),
				// Nullable server-side and three-state on a patch, so a cleared box travels as null.
				description: f.description.trim() === '' ? null : f.description.trim(),
				unit: f.unit.trim(),
				unitPrice: { amount, currency },
				vatCode: f.vatCode
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
				<Field
					label={t('services.code')}
					htmlFor="svc-code"
					required
					error={missing('code')}
				>
					<Input
						value={f.code}
						onBlur={blur('code')}
						onChange={(e) => set('code', e.target.value)}
					/>
				</Field>
				<Field
					label={t('services.unit')}
					htmlFor="svc-unit"
					required
					error={missing('unit')}
				>
					<Input
						value={f.unit}
						onBlur={blur('unit')}
						onChange={(e) => set('unit', e.target.value)}
					/>
				</Field>
			</div>

			<Field label={t('common.name')} htmlFor="svc-name" required error={missing('name')}>
				<Input
					value={f.name}
					onBlur={blur('name')}
					onChange={(e) => set('name', e.target.value)}
				/>
			</Field>

			<Field label={t('services.description')} htmlFor="svc-desc" hint={t('common.optional')}>
				<Input value={f.description} onChange={(e) => set('description', e.target.value)} />
			</Field>

			<div className="grid gap-4 sm:grid-cols-2">
				<Field
					label={t('services.unitPrice')}
					htmlFor="svc-price"
					required
					hint={t('services.priceHint', { currency })}
					error={priceError}
				>
					<Input
						inputMode="decimal"
						value={f.unitPrice}
						onBlur={blur('unitPrice')}
						onChange={(e) => set('unitPrice', e.target.value)}
					/>
				</Field>
				<Field label={t('services.vatCode')} htmlFor="svc-vat" required>
					<Select value={f.vatCode} onChange={(e) => set('vatCode', e.target.value)}>
						{VAT_CODES.map((c) => (
							<option key={c} value={c}>
								{c}
							</option>
						))}
					</Select>
				</Field>
			</div>

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
