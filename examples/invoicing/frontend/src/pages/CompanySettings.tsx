import * as React from 'react'
import { useLocation } from 'react-router-dom'

import type { SecretStatus, SellerView } from '@mintworks/client'
import {
	ServerError,
	useAuth,
	useNavCredentials,
	useSeller,
	useSetSellerClosed,
	useSetSellerPaymentDays,
	useSyncSeller
} from '@mintworks/client'
import { ConfirmDialog, StepUpDialog } from '~/components/ConfirmDialog'
import { Modal } from '~/components/Modal'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, PageSpinner, StatusChip } from '~/components/ui'
import { useT } from '~/i18n'
import {
	type Company,
	NavCredentialsForm,
	SellerFields,
	companyErrors,
	companyOf,
	sellerBody,
	serverFieldErrors
} from '~/pages/Onboarding'

export function CompanySettings() {
	const { t, err } = useT()
	const seller = useSeller()
	const { me } = useAuth()
	const { hash } = useLocation()

	// The warning band links to `#nav`, and a client-side route change does not scroll to it.
	React.useEffect(() => {
		if (hash === '#nav' && seller.data) document.getElementById('nav')?.scrollIntoView()
	}, [hash, seller.data])

	if (seller.isPending) return <PageSpinner />
	if (seller.error) return <ErrorBanner message={err(seller.error)} />

	return (
		<div className="space-y-10">
			<h1 className="text-lg font-semibold text-fg">{t('company.title')}</h1>
			<CompanyDetails
				key={seller.data.sellerVer}
				saved={companyOf(seller.data)}
				taxLocked={seller.data.taxNumberLocked}
				closed={seller.data.closedAt !== null}
			/>
			<PaymentTerms key={seller.data.paymentDays ?? 'default'} seller={seller.data} />
			<NavConnection />
			{me?.org?.role === 'OWNER' && !seller.data.inherited && (
				<CompanyStatus seller={seller.data} />
			)}
		</div>
	)
}

function CompanyDetails({
	saved,
	taxLocked,
	closed
}: {
	saved: Company
	taxLocked: boolean
	closed: boolean
}) {
	const { t, err, fields } = useT()
	const toast = useToast()
	const sync = useSyncSeller()
	const [value, setValue] = React.useState(saved)
	const [errors, setErrors] = React.useState<Record<string, string>>({})
	const [banner, setBanner] = React.useState<string | null>(null)
	const dirty = JSON.stringify(sellerBody(value)) !== JSON.stringify(sellerBody(saved))

	async function save() {
		setBanner(null)
		// A pristine form sends nothing: a Save is not a reason to publish a version.
		if (!dirty) {
			toast.info(t('company.noChanges'))
			return
		}
		const local = companyErrors(value)
		// A legacy tax number failing today's checksum must not block saving the other fields.
		if (taxLocked) delete local.taxNumber
		if (Object.keys(local).length > 0) {
			setErrors(Object.fromEntries(Object.entries(local).map(([k, v]) => [k, t(v)])))
			return
		}
		setErrors({})
		try {
			// `null` is the server's 204: normalised, the edit equals what is live.
			const view = await sync.mutateAsync(sellerBody(value))
			toast[view ? 'success' : 'info'](view ? t('common.saved') : t('company.noChanges'))
		} catch (e) {
			const f = serverFieldErrors(e, fields, err)
			if (Object.keys(f).length > 0) setErrors(f)
			else setBanner(err(e))
		}
	}

	return (
		<section aria-labelledby="company-details">
			<h2 id="company-details" className="text-base font-semibold text-fg">
				{t('company.details')}
			</h2>
			<form
				noValidate
				className="mt-4 max-w-xl rounded-xl border border-line bg-surface-raised p-5"
				onSubmit={(e) => {
					e.preventDefault()
					void save()
				}}
			>
				<ErrorBanner message={banner} />
				<fieldset disabled={closed} className="mt-2">
					<SellerFields
						value={value}
						onChange={setValue}
						errors={errors}
						locked
						taxLocked={taxLocked}
					/>
				</fieldset>
				{!closed && (
					<>
						<Button type="submit" className="mt-6" loading={sync.isPending}>
							{t('company.save')}
						</Button>
						<p className="mt-2 text-xs text-fg-muted">{t('company.save.hint')}</p>
					</>
				)}
			</form>
		</section>
	)
}

function NavConnection() {
	const { t, err } = useT()
	const status = useNavCredentials()
	const [editing, setEditing] = React.useState(false)

	return (
		<section id="nav" aria-labelledby="nav-heading" className="max-w-xl">
			<h2 id="nav-heading" className="text-base font-semibold text-fg">
				{t('navConn.section')}
			</h2>
			{status.isPending ? (
				<PageSpinner />
			) : status.error ? (
				<ErrorBanner message={err(status.error)} />
			) : (
				<div className="mt-4 space-y-4 rounded-xl border border-line bg-surface-raised p-5 text-sm">
					<p>
						{status.data.connected ? (
							<StatusChip tone="positive">
								{t('navConn.connectedAs', { login: status.data.login ?? '' })}
							</StatusChip>
						) : (
							<StatusChip tone="warning">{t('navConn.notConnected')}</StatusChip>
						)}
					</p>
					<dl className="space-y-1">
						<KeyRow label={t('navConn.password')} s={status.data.techPassword} />
						<KeyRow label={t('navConn.signKey')} s={status.data.signKey} />
						<KeyRow label={t('navConn.exchangeKey')} s={status.data.exchangeKey} />
					</dl>
					<Button
						variant={status.data.connected ? 'secondary' : 'primary'}
						onClick={() => setEditing(true)}
					>
						{t('navConn.change')}
					</Button>
				</div>
			)}
			<Modal open={editing} onClose={() => setEditing(false)} title={t('navConn.title')}>
				{editing && (
					<NavCredentialsForm
						onDone={() => setEditing(false)}
						secondary={{ label: t('common.cancel'), onClick: () => setEditing(false) }}
					/>
				)}
			</Modal>
		</section>
	)
}

/** The org's default payment term, which a partner's own overrides. Read-only when the
 *  seller is an ancestor's. */
function PaymentTerms({ seller }: { seller: SellerView }) {
	const { t, err } = useT()
	const toast = useToast()
	const save = useSetSellerPaymentDays()
	const [days, setDays] = React.useState(seller.paymentDays?.toString() ?? '')
	const fallback = t('company.paymentDays.fallback', { n: String(seller.defaultPaymentDays) })

	return (
		<section aria-labelledby="company-terms" className="max-w-xl">
			<h2 id="company-terms" className="text-base font-semibold text-fg">
				{t('company.paymentDays')}
			</h2>
			<div className="mt-4 space-y-4 rounded-xl border border-line bg-surface-raised p-5 text-sm">
				{seller.inherited ? (
					<p>{seller.paymentDays ?? fallback}</p>
				) : (
					<form
						className="flex flex-wrap items-end gap-2"
						onSubmit={(e) => {
							e.preventDefault()
							save.mutateAsync(days.trim() === '' ? null : Number(days))
								.then(() => toast.success(t('company.paymentDays.saved')))
								.catch((e2) => toast.error(err(e2)))
						}}
					>
						<div className="w-64">
							<Field label={t('company.paymentDays.days')} htmlFor="company-days">
								<Input
									type="number"
									min={0}
									max={36500}
									placeholder={fallback}
									value={days}
									onChange={(e) => setDays(e.target.value)}
								/>
							</Field>
						</div>
						<Button type="submit" variant="secondary" loading={save.isPending}>
							{t('common.save')}
						</Button>
					</form>
				)}
			</div>
		</section>
	)
}

/** Owner only: make the company read-only, or reopen it. Both are step-up gated. */
function CompanyStatus({ seller }: { seller: SellerView }) {
	const { t, err, date } = useT()
	const toast = useToast()
	const setClosed = useSetSellerClosed()
	const [confirming, setConfirming] = React.useState(false)
	const [stepUp, setStepUp] = React.useState<boolean | null>(null)
	const closed = seller.closedAt !== null

	async function run(close: boolean) {
		try {
			await setClosed.mutateAsync(close)
			setStepUp(null)
			toast.success(t(close ? 'company.status.closedDone' : 'company.status.reopenedDone'))
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(close)
				return
			}
			setStepUp(null)
			toast.error(err(e))
		}
	}

	return (
		<section aria-labelledby="company-status" className="max-w-xl">
			<h2 id="company-status" className="text-base font-semibold text-fg">
				{t('company.status')}
			</h2>
			<div className="mt-4 space-y-4 rounded-xl border border-line bg-surface-raised p-5 text-sm">
				{closed ? (
					<>
						<p>{t('company.status.closedSince', { date: date(seller.closedAt) })}</p>
						<Button
							variant="secondary"
							loading={setClosed.isPending}
							onClick={() => void run(false)}
						>
							{t('company.status.reopen')}
						</Button>
					</>
				) : (
					<>
						<p className="text-fg-muted">{t('company.status.activeHint')}</p>
						<Button variant="danger" onClick={() => setConfirming(true)}>
							{t('company.status.close')}
						</Button>
					</>
				)}
			</div>
			<ConfirmDialog
				open={confirming}
				title={t('company.status.confirmTitle')}
				description={t('company.status.confirmBody')}
				confirmLabel={t('company.status.close')}
				loading={setClosed.isPending}
				onClose={() => setConfirming(false)}
				onConfirm={() => {
					setConfirming(false)
					void run(true)
				}}
			/>
			<StepUpDialog
				open={stepUp !== null}
				onClose={() => setStepUp(null)}
				onAuthenticated={() => {
					const pending = stepUp
					setStepUp(null)
					if (pending !== null) void run(pending)
				}}
			/>
		</section>
	)
}

function KeyRow({ label, s }: { label: string; s: SecretStatus }) {
	const { t } = useT()
	return (
		<div className="flex justify-between gap-4">
			<dt className="text-fg-muted">{label}</dt>
			<dd className="text-fg">{s.set ? t('navConn.key.set') : t('navConn.key.unset')}</dd>
		</div>
	)
}

// vim: ts=4
