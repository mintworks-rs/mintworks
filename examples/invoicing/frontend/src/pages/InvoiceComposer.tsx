// SPDX-License-Identifier: MIT-0
import type * as React from 'react'
import { useState } from 'react'
import { Navigate, useNavigate, useParams } from 'react-router-dom'

import { useQueryClient } from '@tanstack/react-query'

import type { InvoiceView, LineView } from '@mintworks/client'
import {
	keys,
	localDate,
	useAuth,
	useCurrencies,
	useInvoice,
	useNavCredentials,
	useParties,
	useSeller,
	useServices,
	vatRate
} from '@mintworks/client'
import {
	useAddLine,
	useCreateDraft,
	useDeleteDraft,
	useEditLine,
	useIssueInvoice,
	usePatchInvoice,
	useRemoveLine
} from '~/api/hooks'
import { ConfirmDialog } from '~/components/ConfirmDialog'
import { Modal } from '~/components/Modal'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, MoneyText, PageSpinner, Select } from '~/components/ui'
import { type Key, useT } from '~/i18n'
import { VAT_CODES } from '~/pages/Services'

interface Header {
	partyUid: string
	currency: string
	paymentMethod: string
	fulfilmentDate: string
	dueDate: string
	notes: string
}

function headerOf(inv: InvoiceView | null, fallbackCurrency: string): Header {
	return {
		partyUid: inv?.billingPartyUid ?? '',
		currency: inv?.currency ?? fallbackCurrency,
		paymentMethod: inv?.paymentMethod ?? 'TRANSFER',
		fulfilmentDate: inv?.fulfilmentDate ?? '',
		dueDate: inv?.dueDate ?? '',
		notes: inv?.notes ?? ''
	}
}

function body(h: Header): Record<string, unknown> {
	return {
		partyUid: h.partyUid === '' ? null : h.partyUid,
		currency: h.currency,
		paymentMethod: h.paymentMethod,
		fulfilmentDate: h.fulfilmentDate === '' ? null : h.fulfilmentDate,
		dueDate: h.dueDate === '' ? null : h.dueDate,
		notes: h.notes === '' ? null : h.notes
	}
}

function addDays(iso: string, days: number): string {
	const d = new Date(`${iso}T00:00:00Z`)
	d.setUTCDate(d.getUTCDate() + days)
	return d.toISOString().slice(0, 10)
}

/** A cash total rounded to the currency's `cashRoundStep` (minor units), halves away from zero.
 *  Display only: the PDF's payable is the authoritative figure. */
function cashRound(amount: string, step: number): string {
	const [int, frac = ''] = amount.split('.')
	const minor = Number(int + frac)
	const abs = Math.abs(minor)
	const rounded = Math.floor((2 * abs + step) / (2 * step)) * step
	const digits = String(rounded).padStart(frac.length + 1, '0')
	const out =
		frac.length === 0
			? digits
			: `${digits.slice(0, -frac.length)}.${digits.slice(-frac.length)}`
	return minor < 0 ? `-${out}` : out
}

const DUE_PRESETS = [0, 8, 15, 30]

export function InvoiceComposer() {
	const { uid = '' } = useParams()
	const { err } = useT()
	const inv = useInvoice(uid)

	if (uid === '') return <Composer key="new" invoice={null} />
	if (inv.isPending) return <PageSpinner />
	if (inv.error) return <ErrorBanner message={err(inv.error)} />
	if (!inv.data) return null
	// Only a DRAFT has a composer: an issued invoice is immutable and can only be stornoed.
	if (inv.data.status !== 'DRAFT') return <Navigate to={`/invoices/${uid}`} replace />
	// Keyed, so the form state is seeded from the row by the remount rather than by an effect.
	return <Composer key={uid} invoice={inv.data} />
}

function Composer({ invoice }: { invoice: InvoiceView | null }) {
	const { t, err, qty, date } = useT()
	const navigate = useNavigate()
	const toast = useToast()
	const qc = useQueryClient()
	const parties = useParties()
	const currencies = useCurrencies()
	const seller = useSeller()
	const uid = invoice?.uid ?? ''

	const create = useCreateDraft()
	const patch = usePatchInvoice(uid)
	const discard = useDeleteDraft()
	const issue = useIssueInvoice()
	const nav = useNavCredentials()
	const addLine = useAddLine(uid)
	const removeLine = useRemoveLine(uid)

	const { me } = useAuth()
	// A new draft opens in the org's billing currency, which is what the server would have
	// defaulted to anyway — the POST carries the field, so guessing HUF here would override it.
	const [h, setH] = useState<Header>(() => headerOf(invoice, me?.org?.billingCurrency ?? 'HUF'))
	const [lineOpen, setLineOpen] = useState(false)
	const [editing, setEditing] = useState<LineView | null>(null)
	const [deleting, setDeleting] = useState(false)
	const [review, setReview] = useState(false)
	const [error, setError] = useState<string | null>(null)
	const [issueError, setIssueError] = useState<string | null>(null)

	const lines = invoice?.lines ?? []

	/**
	 * The draft is born on the first meaningful input — a party picked, or a first line added
	 * — and the route is *replaced* so Back does not land on an empty composer. Everything
	 * typed into the header before that rides along in the POST.
	 */
	async function ensureDraft(line?: Record<string, unknown>, header: Header = h) {
		setError(null)
		try {
			if (invoice) {
				if (line) await addLine.mutateAsync(line)
				return
			}
			const made = await create.mutateAsync({ ...body(header), lines: line ? [line] : [] })
			qc.setQueryData(keys.invoice(made.uid), made)
			navigate(`/invoices/${made.uid}/edit`, { replace: true })
		} catch (e) {
			// A line's failure belongs to the open LineForm; the composer's banner is behind it.
			if (line) throw e
			setError(err(e))
		}
	}

	/** Several header fields in one PATCH, so a method and the dates it clears land together. */
	function commit(changes: Partial<Header>) {
		const next = { ...h, ...changes }
		setH(next)
		if (!invoice) {
			if (changes.partyUid) void ensureDraft(undefined, next)
			return
		}
		const stored = headerOf(invoice, h.currency)
		const diff = Object.fromEntries(
			Object.entries(changes)
				.filter(([k, v]) => stored[k as keyof Header] !== v)
				.map(([k, v]) => [k, v === '' ? null : v])
		)
		if (Object.keys(diff).length === 0) return
		patch.mutateAsync(diff).catch((e) => setError(err(e)))
	}

	/** CASH is dated and paid on issue, so choosing it clears both dates in the same PATCH. */
	const withMethod = (m: string): Partial<Header> =>
		m === 'CASH' ? { paymentMethod: m, fulfilmentDate: '', dueDate: '' } : { paymentMethod: m }

	function pickParty(partyUid: string) {
		const method = parties.data?.items.find((p) => p.uid === partyUid)?.paymentMethod
		commit({ partyUid, ...(method && method !== h.paymentMethod ? withMethod(method) : {}) })
	}

	const field = (k: keyof Header) => ({
		value: h[k],
		onChange: (e: React.ChangeEvent<HTMLInputElement>) =>
			setH((p) => ({ ...p, [k]: e.target.value })),
		onBlur: (e: React.FocusEvent<HTMLInputElement>) => commit({ [k]: e.target.value })
	})
	const picker = (k: keyof Header) => ({
		value: h[k],
		onChange: (e: React.ChangeEvent<HTMLSelectElement>) => commit({ [k]: e.target.value })
	})

	// A preview of what the server decides at issue: party → company → deployment default.
	const party = parties.data?.items.find((p) => p.uid === h.partyUid)
	const cash = h.paymentMethod === 'CASH'
	const today = localDate()
	const base = h.fulfilmentDate || today
	const terms =
		party?.paymentDays != null
			? { days: party.paymentDays, source: 'partner' as const }
			: seller.data?.paymentDays != null
				? { days: seller.data.paymentDays, source: 'company' as const }
				: seller.data
					? { days: seller.data.defaultPaymentDays, source: 'default' as const }
					: null
	const autoDue = cash ? today : terms ? addDays(base, terms.days) : undefined
	const cashStep = cash
		? (currencies.data?.items.find((c) => c.code === h.currency)?.cashRoundStep ?? null)
		: null

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-start justify-between gap-4">
				<div>
					<h1 className="text-lg font-semibold text-fg">
						{invoice ? t('composer.editTitle') : t('composer.newTitle')}
					</h1>
					<p className="mt-1 max-w-2xl text-sm text-fg-muted">{t('composer.intro')}</p>
				</div>
				<div className="flex gap-2">
					{invoice && (
						<Button variant="danger" onClick={() => setDeleting(true)}>
							{t('composer.delete')}
						</Button>
					)}
					<Button
						disabled={!invoice || lines.length === 0}
						onClick={() => {
							setIssueError(null)
							setReview(true)
						}}
					>
						{t('composer.issue')}
					</Button>
				</div>
			</div>

			<ErrorBanner message={error} />

			<section className="grid gap-4 rounded-xl border border-line bg-surface-raised p-4 sm:grid-cols-2">
				<Field label={t('invoices.buyer')} htmlFor="inv-party" required>
					<Select value={h.partyUid} onChange={(e) => pickParty(e.target.value)}>
						<option value="">{t('composer.pickParty')}</option>
						{(parties.data?.items ?? []).map((p) => (
							<option key={p.uid} value={p.uid}>
								{p.name}
							</option>
						))}
					</Select>
				</Field>
				<Field label={t('invoices.currency')} htmlFor="inv-currency">
					<Select {...picker('currency')}>
						{(currencies.data?.items ?? [])
							.filter((c) => c.enabled || c.code === h.currency)
							.map((c) => (
								<option key={c.code} value={c.code}>
									{c.code}
								</option>
							))}
						{currencies.data === undefined && (
							<option value={h.currency}>{h.currency}</option>
						)}
					</Select>
				</Field>
				{h.paymentMethod === 'CARD' ? (
					<Field
						label={t('invoices.paymentMethod')}
						htmlFor="inv-pay"
						hint={t('composer.cardHint')}
					>
						{/* CARD is stamped by the payment flow alone, so it is shown, never sent. */}
						<Select value={h.paymentMethod} disabled>
							<option value="CARD">{t('invoices.pay.CARD')}</option>
						</Select>
					</Field>
				) : (
					<Field
						label={t('invoices.paymentMethod')}
						htmlFor="inv-pay"
						hint={cash ? t('composer.cashDates') : undefined}
					>
						<Select
							value={h.paymentMethod}
							onChange={(e) => commit(withMethod(e.target.value))}
						>
							{['TRANSFER', 'CASH'].map((m) => (
								<option key={m} value={m}>
									{t(`invoices.pay.${m}` as Key)}
								</option>
							))}
						</Select>
					</Field>
				)}
				<div />
				<Field
					label={t('invoices.fulfilment')}
					htmlFor="inv-fulfil"
					hint={cash ? undefined : t('composer.fulfilmentHint')}
				>
					<Input type="date" disabled={cash} {...field('fulfilmentDate')} />
				</Field>
				<div className="flex flex-col gap-2">
					<Field
						label={t('invoices.due')}
						htmlFor="inv-due"
						hint={
							!cash && h.dueDate === '' && terms && autoDue
								? t('composer.due.auto', {
										date: date(autoDue),
										days: String(terms.days),
										source: t(`composer.due.source.${terms.source}`)
									})
								: undefined
						}
					>
						<Input type="date" disabled={cash} {...field('dueDate')} />
					</Field>
					{!cash && (
						<div className="flex flex-wrap gap-2">
							{DUE_PRESETS.map((n) => {
								const due = addDays(base, n)
								const pressed = h.dueDate === due
								return (
									<Button
										key={n}
										type="button"
										variant="secondary"
										className={pressed ? 'border-accent text-accent' : ''}
										aria-pressed={pressed}
										onClick={() => commit({ dueDate: due })}
									>
										{n === 0
											? t('composer.due.preset.now')
											: t('composer.due.preset.days', { n: String(n) })}
									</Button>
								)
							})}
							{h.dueDate !== '' && (
								<Button
									type="button"
									variant="ghost"
									onClick={() => commit({ dueDate: '' })}
								>
									{t('composer.due.useDefault')}
								</Button>
							)}
						</div>
					)}
				</div>
				<div className="sm:col-span-2">
					<Field
						label={t('invoices.notes')}
						htmlFor="inv-notes"
						hint={t('common.optional')}
					>
						<Input {...field('notes')} />
					</Field>
				</div>
			</section>

			<section className="space-y-3">
				<div className="flex items-center justify-between">
					<h2 className="text-sm font-semibold text-fg">{t('composer.lines')}</h2>
					<Button
						// Primary only while there are no lines: Issue stays disabled until then.
						variant={lines.length === 0 ? 'primary' : 'secondary'}
						onClick={() => {
							setEditing(null)
							setLineOpen(true)
						}}
					>
						{t('composer.addLine')}
					</Button>
				</div>

				{lines.length === 0 ? (
					<p className="rounded-xl border border-dashed border-line-strong p-6 text-center text-sm text-fg-muted">
						{t('composer.noLines')}
					</p>
				) : (
					<div className="overflow-x-auto rounded-xl border border-line bg-surface-raised">
						<table className="w-full text-sm">
							<caption className="sr-only">{t('composer.lines')}</caption>
							<thead className="border-b border-line bg-surface-sunken text-left text-fg-muted">
								<tr>
									<th scope="col" className="px-4 py-3 font-medium">
										{t('composer.lineDescription')}
									</th>
									<th scope="col" className="px-4 py-3 text-right font-medium">
										{t('composer.qty')}
									</th>
									<th scope="col" className="px-4 py-3 text-right font-medium">
										{t('services.unitPrice')}
									</th>
									<th scope="col" className="px-4 py-3 text-right font-medium">
										{t('services.vatCode')}
									</th>
									<th scope="col" className="px-4 py-3 text-right font-medium">
										{t('invoices.net')}
									</th>
									<th scope="col" className="px-4 py-3 text-right font-medium">
										{t('common.actions')}
									</th>
								</tr>
							</thead>
							<tbody className="divide-y divide-line">
								{lines.map((l) => (
									<tr key={l.lineNo} className="h-10">
										<td className="px-4 py-2.5 text-fg">{l.description}</td>
										<td className="px-4 py-2.5 text-right tnum text-fg">
											{qty(l.qty)} {l.unit}
										</td>
										<td className="px-4 py-2.5 text-right tnum text-fg">
											<MoneyText value={l.unitPrice} />
										</td>
										<td className="px-4 py-2.5 text-right text-fg">
											{l.vatCode}
										</td>
										<td className="px-4 py-2.5 text-right tnum text-fg">
											<MoneyText value={l.net} />
										</td>
										<td className="px-4 py-2.5 text-right">
											<span className="flex justify-end gap-1">
												<Button
													variant="ghost"
													onClick={() => {
														setEditing(l)
														setLineOpen(true)
													}}
												>
													{t('common.edit')}
												</Button>
												<Button
													variant="ghost"
													onClick={() =>
														void removeLine
															.mutateAsync(l.lineNo)
															.catch((e) => setError(err(e)))
													}
												>
													{t('composer.removeLine')}
												</Button>
											</span>
										</td>
									</tr>
								))}
							</tbody>
						</table>
					</div>
				)}
			</section>

			{invoice && <Totals invoice={invoice} cashStep={cashStep} />}

			<Modal
				open={lineOpen}
				onClose={() => setLineOpen(false)}
				title={editing ? t('composer.editLine') : t('composer.addLine')}
				className="w-[min(40rem,calc(100vw-2rem))]"
			>
				{lineOpen && (
					<LineForm
						key={editing?.lineNo ?? 'new'}
						uid={uid}
						line={editing}
						currency={h.currency}
						onDone={() => setLineOpen(false)}
						onCreate={(line) => ensureDraft(line)}
						creating={addLine.isPending || create.isPending}
					/>
				)}
			</Modal>

			<Modal
				open={review}
				onClose={() => setReview(false)}
				title={t('composer.review')}
				className="w-[min(48rem,calc(100vw-2rem))]"
			>
				{invoice && (
					<div className="space-y-4">
						<Review invoice={invoice} autoDue={autoDue} cashStep={cashStep} />
						<p className="rounded-md border border-warning px-3 py-2 text-sm text-fg">
							{t('composer.immutable')}
						</p>
						{nav.data?.connected === false && (
							<p className="text-sm text-fg-muted">{t('navBand.notConnected')}</p>
						)}
						<ErrorBanner message={issueError} />
						<div className="flex justify-end gap-2">
							<Button variant="secondary" onClick={() => setReview(false)}>
								{t('common.cancel')}
							</Button>
							<Button
								loading={issue.isPending}
								onClick={() =>
									void issue
										.mutateAsync(invoice.uid)
										.then(() => {
											toast.success(t('composer.issued'))
											navigate(`/invoices/${invoice.uid}`, { replace: true })
										})
										.catch((e) => setIssueError(err(e)))
								}
							>
								{t('composer.issue')}
							</Button>
						</div>
					</div>
				)}
			</Modal>

			<ConfirmDialog
				open={deleting}
				title={t('composer.delete.title')}
				description={t('composer.delete.body')}
				confirmLabel={t('composer.delete')}
				loading={discard.isPending}
				onClose={() => setDeleting(false)}
				onConfirm={() => {
					if (!invoice) return
					void discard
						.mutateAsync(invoice.uid)
						.then(() => {
							toast.success(t('composer.deleted'))
							navigate('/invoices')
						})
						.catch((e) => setError(err(e)))
						.finally(() => setDeleting(false))
				}}
			/>
		</div>
	)
}

/** The server's figures, never the client's: VAT is computed once per rate group on the summed
 *  net, and a per-line client total would disagree with the document. */
function Totals({ invoice, cashStep }: { invoice: InvoiceView; cashStep?: number | null }) {
	const { t } = useT()
	return (
		<section className="rounded-xl border border-line bg-surface-raised p-4">
			<h2 className="text-sm font-semibold text-fg">{t('composer.totals')}</h2>
			<table className="mt-3 w-full text-sm">
				<caption className="sr-only">{t('composer.totals')}</caption>
				<thead className="text-left text-fg-muted">
					<tr>
						<th scope="col" className="py-1 font-medium">
							{t('services.vatCode')}
						</th>
						<th scope="col" className="py-1 text-right font-medium">
							{t('invoices.net')}
						</th>
						<th scope="col" className="py-1 text-right font-medium">
							{t('invoices.vat')}
						</th>
						<th scope="col" className="py-1 text-right font-medium">
							{t('invoices.gross')}
						</th>
					</tr>
				</thead>
				<tbody>
					{(invoice.vatSummary ?? []).map((g) => (
						<tr key={g.vatCode}>
							<td className="py-1 text-fg">
								{g.vatCode} ({vatRate(g.vatRateBp)})
							</td>
							<td className="py-1 text-right tnum text-fg">
								<MoneyText value={g.net} />
							</td>
							<td className="py-1 text-right tnum text-fg">
								<MoneyText value={g.vat} />
							</td>
							<td className="py-1 text-right tnum text-fg">
								<MoneyText value={g.gross} />
							</td>
						</tr>
					))}
					<tr className="border-t border-line font-medium">
						<td className="py-2 text-fg">{t('composer.total')}</td>
						<td className="py-2 text-right tnum text-fg">
							<MoneyText value={invoice.net} />
						</td>
						<td className="py-2 text-right tnum text-fg">
							<MoneyText value={invoice.vat} />
						</td>
						<td className="py-2 text-right tnum text-fg">
							<MoneyText value={invoice.gross} />
						</td>
					</tr>
					{cashStep != null && (
						<tr>
							<td className="py-1 text-fg-muted" colSpan={3}>
								{t('composer.payable')}
							</td>
							<td className="py-1 text-right tnum text-fg">
								<MoneyText
									value={{
										...invoice.gross,
										amount: cashRound(invoice.gross.amount, cashStep)
									}}
								/>
							</td>
						</tr>
					)}
				</tbody>
			</table>
		</section>
	)
}

/** The read-only render the issue step shows: what the document will say, before it becomes
 *  a document. `autoDue` is the composer's preview of an unset due date; the detail page of an
 *  issued invoice passes neither prop. */
export function Review({
	invoice,
	autoDue,
	cashStep
}: {
	invoice: InvoiceView
	autoDue?: string
	cashStep?: number | null
}) {
	const { t, date, qty } = useT()
	const cashDraft = autoDue !== undefined && invoice.paymentMethod === 'CASH'
	return (
		<div className="space-y-4 text-sm">
			<dl className="grid gap-x-6 gap-y-1 sm:grid-cols-2">
				<div>
					<dt className="text-fg-muted">{t('invoices.buyer')}</dt>
					<dd className="text-fg">{invoice.buyer?.name ?? t('common.none')}</dd>
					<dd className="text-fg-muted">
						{[invoice.buyer?.postcode, invoice.buyer?.city, invoice.buyer?.street]
							.filter(Boolean)
							.join(' ')}
					</dd>
					<dd className="text-fg-muted">{invoice.buyer?.taxNumber ?? ''}</dd>
				</div>
				<div>
					<dt className="text-fg-muted">{t('invoices.paymentMethod')}</dt>
					<dd className="text-fg">{t(`invoices.pay.${invoice.paymentMethod}` as Key)}</dd>
					<dt className="mt-1 text-fg-muted">{t('invoices.fulfilment')}</dt>
					<dd className="text-fg">
						{invoice.fulfilmentDate || autoDue === undefined
							? date(invoice.fulfilmentDate)
							: `${date(localDate())} (${t('composer.due.autoLabel')})`}
					</dd>
					<dt className="mt-1 text-fg-muted">{t('invoices.due')}</dt>
					<dd className="text-fg">
						{cashDraft
							? t('composer.cashToday')
							: invoice.dueDate
								? date(invoice.dueDate)
								: autoDue
									? `${date(autoDue)} (${t('composer.due.autoLabel')})`
									: date(null)}
					</dd>
				</div>
			</dl>

			<ul className="divide-y divide-line rounded-md border border-line">
				{(invoice.lines ?? []).map((l) => (
					<li key={l.lineNo} className="flex justify-between gap-4 px-3 py-2">
						<span className="text-fg">
							{l.description}
							<span className="text-fg-muted">
								{' '}
								· {qty(l.qty)} {l.unit} · {l.vatCode}
							</span>
						</span>
						<MoneyText value={l.net} />
					</li>
				))}
			</ul>

			<Totals invoice={invoice} cashStep={cashStep} />
		</div>
	)
}

interface LineDraft {
	serviceCode: string
	description: string
	unit: string
	qty: string
	unitPrice: string
	vatCode: string
}

/**
 * Two line kinds, as the API models them: a **catalogue** line carries `serviceCode` and takes
 * price, unit, description and VAT code from the `services` row — supplying a price as well is
 * refused — and an ad-hoc line needs all four. On an edit the kind is fixed and `unit` is not
 * patchable (`invoices.rn::line_patch`).
 */
function LineForm({
	uid,
	line,
	currency,
	onDone,
	onCreate,
	creating
}: {
	uid: string
	line: LineView | null
	currency: string
	onDone: () => void
	onCreate: (line: Record<string, unknown>) => Promise<void>
	creating: boolean
}) {
	const { t, err } = useT()
	const services = useServices()
	const edit = useEditLine(uid)
	// Ad-hoc when the catalogue is empty: a picker with nothing to pick is a dead end.
	const [picked, setCatalogue] = useState<boolean | null>(null)
	const catalogue = picked ?? (line === null && services.data?.items.length !== 0)
	const [f, setF] = useState<LineDraft>({
		serviceCode: '',
		description: line?.description ?? '',
		unit: line?.unit ?? 'db',
		qty: line?.qty ?? '1',
		unitPrice: line?.unitPrice.amount ?? '',
		vatCode: line?.vatCode ?? 'STD27'
	})
	const [error, setError] = useState<string | null>(null)
	const set = (k: keyof LineDraft, v: string) => setF((p) => ({ ...p, [k]: v }))

	const incomplete = catalogue
		? f.serviceCode === '' || f.qty.trim() === ''
		: f.description.trim() === '' || f.unit.trim() === '' || f.qty.trim() === ''

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		try {
			if (line) {
				await edit.mutateAsync({
					lineNo: line.lineNo,
					patch: {
						description: f.description.trim(),
						qty: f.qty.trim(),
						vatCode: f.vatCode,
						...(f.unitPrice.trim() === ''
							? {}
							: { unitPrice: { amount: f.unitPrice.trim(), currency } })
					}
				})
			} else if (catalogue) {
				await onCreate({ serviceCode: f.serviceCode, qty: f.qty.trim() })
			} else {
				await onCreate({
					description: f.description.trim(),
					unit: f.unit.trim(),
					qty: f.qty.trim(),
					unitPrice: { amount: f.unitPrice.trim(), currency },
					vatCode: f.vatCode
				})
			}
			onDone()
		} catch (e2) {
			setError(err(e2))
		}
	}

	return (
		<form onSubmit={submit} className="space-y-4">
			<ErrorBanner message={error} />

			{line === null && (
				<div className="flex gap-2">
					<Button
						type="button"
						variant="secondary"
						className={catalogue ? 'border-accent text-accent' : ''}
						aria-pressed={catalogue}
						onClick={() => setCatalogue(true)}
					>
						{t('composer.kind.catalogue')}
					</Button>
					<Button
						type="button"
						variant="secondary"
						className={catalogue ? '' : 'border-accent text-accent'}
						aria-pressed={!catalogue}
						onClick={() => setCatalogue(false)}
					>
						{t('composer.kind.adhoc')}
					</Button>
				</div>
			)}

			{catalogue && line === null ? (
				<Field label={t('nav.services')} htmlFor="line-service" required>
					<Select
						value={f.serviceCode}
						onChange={(e) => set('serviceCode', e.target.value)}
					>
						<option value="">{t('composer.pickService')}</option>
						{(services.data?.items ?? []).map((s) => (
							<option key={s.uid} value={s.code ?? ''}>
								{s.name}
							</option>
						))}
					</Select>
				</Field>
			) : (
				<>
					<Field label={t('composer.lineDescription')} htmlFor="line-desc" required>
						<Input
							value={f.description}
							onChange={(e) => set('description', e.target.value)}
						/>
					</Field>
					<div className="grid gap-4 sm:grid-cols-2">
						<Field
							label={t('services.unit')}
							htmlFor="line-unit"
							required
							hint={line ? t('composer.unitFixed') : undefined}
						>
							<Input
								value={f.unit}
								disabled={line !== null}
								onChange={(e) => set('unit', e.target.value)}
							/>
						</Field>
						<Field
							label={t('services.unitPrice')}
							htmlFor="line-price"
							hint={t('services.priceHint', { currency })}
						>
							<Input
								inputMode="decimal"
								value={f.unitPrice}
								onChange={(e) => set('unitPrice', e.target.value)}
							/>
						</Field>
					</div>
					<Field label={t('services.vatCode')} htmlFor="line-vat" required>
						<Select value={f.vatCode} onChange={(e) => set('vatCode', e.target.value)}>
							{VAT_CODES.map((c) => (
								<option key={c} value={c}>
									{c}
								</option>
							))}
						</Select>
					</Field>
				</>
			)}

			<Field label={t('composer.qty')} htmlFor="line-qty" required>
				<Input
					inputMode="decimal"
					value={f.qty}
					onChange={(e) => set('qty', e.target.value)}
				/>
			</Field>

			<div className="flex justify-end gap-2 pt-2">
				<Button variant="secondary" type="button" onClick={onDone}>
					{t('common.cancel')}
				</Button>
				<Button type="submit" loading={edit.isPending || creating} disabled={incomplete}>
					{t('common.save')}
				</Button>
			</div>
		</form>
	)
}

// vim: ts=4
