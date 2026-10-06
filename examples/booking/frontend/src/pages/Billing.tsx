import type * as React from 'react'
import { useEffect, useRef, useState } from 'react'

import type { BillingParty, PartyKind } from '@mintworks/client'
import { errMsg, useParties, useSaveParty } from '@mintworks/client'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, PageSpinner, Select } from '~/components/ui'

interface Form {
	kind: PartyKind
	name: string
	country: string
	taxNumber: string
	euVatId: string
	postcode: string
	city: string
	street: string
	email: string
}

const EMPTY: Form = {
	kind: 'P',
	name: '',
	country: 'HU',
	taxNumber: '',
	euVatId: '',
	postcode: '',
	city: '',
	street: '',
	email: ''
}

const from = (p: BillingParty): Form => ({
	kind: p.kind,
	name: p.name,
	country: p.country,
	taxNumber: p.taxNumber ?? '',
	euVatId: p.euVatId ?? '',
	postcode: p.postcode ?? '',
	city: p.city ?? '',
	street: p.street ?? '',
	email: p.email ?? ''
})

export function Billing() {
	const toast = useToast()
	const parties = useParties()
	const existing = parties.data?.items.find((p) => p.isDefault) ?? parties.data?.items[0] ?? null
	const save = useSaveParty(existing?.uid ?? null)

	const [form, setForm] = useState<Form>(EMPTY)
	const [error, setError] = useState<string | null>(null)

	// Seeded once per party, not on every `existing` identity: `parties.data` is a new object
	// after any refetch, and re-seeding on it overwrote whatever had been typed since.
	const seeded = useRef<string | null>(null)
	useEffect(() => {
		if (existing && seeded.current !== existing.uid) {
			seeded.current = existing.uid
			setForm(from(existing))
		}
	}, [existing])

	if (parties.isPending) return <PageSpinner />
	if (parties.error) return <ErrorBanner message={errMsg(parties.error)} />

	const set = (k: keyof Form) => (e: React.ChangeEvent<HTMLInputElement>) =>
		setForm((f) => ({ ...f, [k]: e.target.value }))

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		try {
			// Blank optional fields go as null, not "": the server's `Patch` type reads an
			// absent key as "leave alone" and null as "clear".
			await save.mutateAsync({
				...form,
				taxNumber: form.taxNumber.trim() || null,
				euVatId: form.euVatId.trim() || null,
				postcode: form.postcode.trim() || null,
				city: form.city.trim() || null,
				street: form.street.trim() || null,
				email: form.email.trim() || null,
				isDefault: true
			})
			toast.success('Billing details saved.')
		} catch (err) {
			setError(errMsg(err))
		}
	}

	return (
		<div className="space-y-6">
			<div>
				<h1 className="text-lg font-semibold text-slate-900">Billing details</h1>
				<p className="mt-1 text-sm text-slate-600">
					{existing
						? 'These appear on every invoice as the buyer.'
						: 'Required before you can check out — an invoice needs a buyer.'}
				</p>
			</div>

			<form
				onSubmit={submit}
				className="space-y-4 rounded-xl border border-slate-200 bg-white p-5"
			>
				<ErrorBanner message={error} />

				<Field label="I am billing as" htmlFor="kind" required>
					{/* Native <select>: the platform already has a combobox. */}
					<Select
						id="kind"
						value={form.kind}
						onChange={(e) =>
							setForm((f) => ({ ...f, kind: e.target.value as PartyKind }))
						}
					>
						<option value="P">A private person</option>
						<option value="C">A company</option>
					</Select>
				</Field>

				<Field label="Name" htmlFor="name" required>
					<Input id="name" value={form.name} onChange={set('name')} />
				</Field>

				<div className="grid gap-4 sm:grid-cols-2">
					<Field
						label="Country"
						htmlFor="country"
						required
						hint="Two-letter code, e.g. HU."
					>
						<Input id="country" value={form.country} onChange={set('country')} />
					</Field>
					<Field label="Email" htmlFor="email">
						<Input id="email" type="email" value={form.email} onChange={set('email')} />
					</Field>
				</div>

				<div className="grid gap-4 sm:grid-cols-2">
					<Field label="Tax number" htmlFor="taxNumber">
						<Input id="taxNumber" value={form.taxNumber} onChange={set('taxNumber')} />
					</Field>
					<Field label="EU VAT id" htmlFor="euVatId">
						<Input id="euVatId" value={form.euVatId} onChange={set('euVatId')} />
					</Field>
				</div>

				<div className="grid gap-4 sm:grid-cols-3">
					<Field label="Postcode" htmlFor="postcode">
						<Input id="postcode" value={form.postcode} onChange={set('postcode')} />
					</Field>
					<Field label="City" htmlFor="city">
						<Input id="city" value={form.city} onChange={set('city')} />
					</Field>
					<Field label="Street" htmlFor="street">
						<Input id="street" value={form.street} onChange={set('street')} />
					</Field>
				</div>

				<Button type="submit" loading={save.isPending}>
					{existing ? 'Save' : 'Save and continue'}
				</Button>
			</form>
		</div>
	)
}

// vim: ts=4
