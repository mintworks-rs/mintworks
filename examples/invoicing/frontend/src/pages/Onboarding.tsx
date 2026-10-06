// SPDX-License-Identifier: MIT-0
import { useQueryClient } from '@tanstack/react-query'
import * as React from 'react'
import { Navigate, useNavigate, useSearchParams } from 'react-router-dom'

import type { NavCredentialsStatus, SellerView } from '@mintworks/client'
import {
	ServerError,
	api,
	useAuth,
	useCreateSeller,
	useSeller,
	useSetNavCredentials
} from '@mintworks/client'
import { StepUpDialog } from '~/components/ConfirmDialog'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, PageSpinner } from '~/components/ui'
import { type Key, useT } from '~/i18n'

// ---------------------------------------------------------------- the gate

/**
 * Keeps an org that cannot invoice yet out of the app: no company workspace active, or one
 * whose seller is not its own. ROOT passes — its seller is the operator's, minted at boot.
 */
export function CompanyGate({ children }: { children: React.ReactNode }) {
	const { me } = useAuth()
	const kind = me?.org?.kind
	const seller = useSeller()
	if (kind === 'ROOT') return <>{children}</>
	if (kind !== 'SHARED') return <Navigate to="/setup" replace />
	if (seller.isPending) return <PageSpinner />
	if (needsSetup(seller.error, seller.data)) return <Navigate to="/setup" replace />
	return <>{children}</>
}

function needsSetup(error: unknown, data: SellerView | undefined): boolean {
	if (error instanceof ServerError && error.httpStatus === 404) return true
	return data?.inherited === true
}

// ---------------------------------------------------------------- company fields

export type VatScheme = '' | 'NORMAL' | 'ALANYI_MENTES'
export type IncomeRegime = 'NONE' | 'KATA' | 'ATALANY'

export interface Company {
	taxNumber: string
	name: string
	postcode: string
	city: string
	street: string
	vatScheme: VatScheme
	incomeRegime: IncomeRegime
	/** The átalány költséghányad; `null` is the year's general rate. */
	expenseRatioPct: number | null
	/** `YYYY-MM-DD`, or empty. */
	regimeSince: string
	bankAccount: string
	bankName: string
	groupMemberTaxNo: string
	euVatId: string
	seriesCode: string
}

export const EMPTY_COMPANY: Company = {
	taxNumber: '',
	name: '',
	postcode: '',
	city: '',
	street: '',
	vatScheme: '',
	incomeRegime: 'NONE',
	expenseRatioPct: null,
	regimeSince: '',
	bankAccount: '',
	bankName: '',
	groupMemberTaxNo: '',
	euVatId: '',
	seriesCode: 'A'
}

export function companyOf(s: SellerView): Company {
	return {
		taxNumber: s.taxNumber,
		name: s.name,
		postcode: s.postcode,
		city: s.city,
		street: s.street,
		vatScheme: s.vatScheme as VatScheme,
		incomeRegime: s.incomeRegime as IncomeRegime,
		expenseRatioPct: s.expenseRatioPct,
		regimeSince: s.regimeSince ?? '',
		bankAccount: s.bankAccount ?? '',
		bankName: s.bankName ?? '',
		groupMemberTaxNo: s.groupMemberTaxNo ?? '',
		euVatId: s.euVatId ?? '',
		seriesCode: s.seriesCode
	}
}

/** The `PUT /api/seller` body. `null` clears an optional field, so an emptied one is cleared. */
export function sellerBody(c: Company): Record<string, unknown> {
	const opt = (v: string) => (v.trim() === '' ? null : v.trim())
	return {
		name: c.name.trim(),
		country: 'HU',
		taxNumber: c.taxNumber.trim(),
		postcode: c.postcode.trim(),
		city: c.city.trim(),
		street: c.street.trim(),
		vatScheme: c.vatScheme,
		incomeRegime: c.incomeRegime,
		// The server refuses a ratio outside átalány, so leaving it drops the ratio too.
		expenseRatioPct: c.incomeRegime === 'ATALANY' ? c.expenseRatioPct : null,
		regimeSince: opt(c.regimeSince),
		bankAccount: opt(c.bankAccount),
		bankName: opt(c.bankName),
		groupMemberTaxNo: opt(c.groupMemberTaxNo),
		euVatId: opt(c.euVatId)
	}
}

/** `12345678-1-12`, typed as digits. */
function maskTaxNumber(v: string): string {
	const d = v.replace(/\D/g, '').slice(0, 11)
	return [d.slice(0, 8), d.slice(8, 9), d.slice(9)].filter(Boolean).join('-')
}

/** `12345678-12345678[-12345678]`, typed as digits. */
function maskBankAccount(v: string): string {
	const d = v.replace(/\D/g, '').slice(0, 24)
	return (d.match(/.{1,8}/g) ?? []).join('-')
}

function taxNumberError(v: string): Key | null {
	const d = v.replace(/\D/g, '')
	if (d === '') return 'common.required'
	// The 9th digit is NAV's `vatCode`, restricted to 1-5.
	if (d.length !== 11 || !/[1-5]/.test(d[8] ?? '')) return 'onboarding.taxNumber.invalid'
	// The 8th digit is the törzsszám check digit, weights 9,7,3,1 over the first seven.
	const sum = [9, 7, 3, 1, 9, 7, 3].reduce((s, w, i) => s + w * Number(d[i]), 0)
	if ((10 - (sum % 10)) % 10 !== Number(d[7])) return 'onboarding.taxNumber.invalid'
	return null
}

/** Client-side refusals, keyed like the wire fields. Empty when the form may be sent. */
export function companyErrors(c: Company): Partial<Record<keyof Company, Key>> {
	const out: Partial<Record<keyof Company, Key>> = {}
	const tax = taxNumberError(c.taxNumber)
	if (tax) out.taxNumber = tax
	for (const f of ['name', 'postcode', 'city', 'street', 'seriesCode'] as const) {
		if (c[f].trim() === '') out[f] = 'common.required'
	}
	if (c.vatScheme === '') out.vatScheme = 'onboarding.vatScheme.required'
	return out
}

/** A server refusal as field errors where it names a field; the rest goes to the banner. */
export function serverFieldErrors(
	e: unknown,
	fields: (e: unknown) => Record<string, string>,
	err: (e: unknown) => string
): Record<string, string> {
	if (!(e instanceof ServerError)) return {}
	if (e.errCode === 'E-INV-SELLER-TAXNUMBER' || e.errCode === 'E-INV-SELLER-TAXNUMBER-LOCKED')
		return { taxNumber: err(e) }
	return fields(e)
}

const VAT_SCHEMES: { value: Exclude<VatScheme, ''>; label: Key; hint: Key }[] = [
	{ value: 'NORMAL', label: 'onboarding.vat.normal', hint: 'onboarding.vat.normal.hint' },
	{ value: 'ALANYI_MENTES', label: 'onboarding.vat.exempt', hint: 'onboarding.vat.exempt.hint' }
]

const INCOME_REGIMES: { value: IncomeRegime; label: Key; hint: Key }[] = [
	{ value: 'NONE', label: 'onboarding.income.none', hint: 'onboarding.income.none.hint' },
	{ value: 'KATA', label: 'onboarding.income.kata', hint: 'onboarding.income.kata.hint' },
	{ value: 'ATALANY', label: 'onboarding.income.atalany', hint: 'onboarding.income.atalany.hint' }
]

/** 40/45/50 are still accepted by the API; the general option covers them year by year. */
const EXPENSE_RATIOS: { value: number | null; label: Key }[] = [
	{ value: null, label: 'onboarding.ratio.general' },
	{ value: 80, label: 'onboarding.ratio.80' },
	{ value: 90, label: 'onboarding.ratio.90' }
]

/** One bordered radio row, the shape every choice in the company form takes. */
function Choice({
	name,
	checked,
	onChange,
	label,
	hint
}: {
	name: string
	checked: boolean
	onChange: () => void
	label: string
	hint?: string
}) {
	return (
		<label className="flex cursor-pointer items-start gap-3 rounded-md border border-line p-3">
			<input
				type="radio"
				name={name}
				className="mt-1 h-4 w-4 accent-accent"
				checked={checked}
				onChange={onChange}
			/>
			<span>
				<span className="block text-sm text-fg">{label}</span>
				{hint && <span className="block text-xs text-fg-muted">{hint}</span>}
			</span>
		</label>
	)
}

/** The company details, shared by `/setup` and the Company settings page. */
export function SellerFields({
	value,
	onChange,
	errors,
	locked = false,
	taxLocked = false
}: {
	value: Company
	onChange: (c: Company) => void
	errors: Record<string, string>
	/** The series code, which is set once. */
	locked?: boolean
	/** The tax number, fixed once an invoice has a number: a new one is a new company. */
	taxLocked?: boolean
}) {
	const { t } = useT()
	const [blurred, setBlurred] = React.useState<string | null>(null)
	const set = (k: keyof Company) => (e: React.ChangeEvent<HTMLInputElement>) =>
		onChange({ ...value, [k]: e.target.value })
	const taxError = errors.taxNumber ?? blurred ?? undefined

	return (
		<div className="space-y-4">
			<Field
				label={t('onboarding.taxNumber')}
				htmlFor="co-tax"
				required
				error={taxError}
				hint={taxLocked ? t('company.taxNumber.locked') : t('onboarding.taxNumber.hint')}
			>
				<Input
					inputMode="numeric"
					autoComplete="off"
					placeholder="12345678-1-12"
					readOnly={taxLocked}
					value={value.taxNumber}
					onChange={(e) => {
						setBlurred(null)
						onChange({ ...value, taxNumber: maskTaxNumber(e.target.value) })
					}}
					onBlur={() => {
						const k = taxLocked ? null : taxNumberError(value.taxNumber)
						setBlurred(k ? t(k) : null)
					}}
				/>
			</Field>
			<Field label={t('onboarding.name')} htmlFor="co-name" required error={errors.name}>
				<Input autoComplete="organization" value={value.name} onChange={set('name')} />
			</Field>
			<div className="grid gap-4 sm:grid-cols-[8rem_1fr]">
				<Field
					label={t('onboarding.postcode')}
					htmlFor="co-postcode"
					required
					error={errors.postcode}
				>
					<Input
						autoComplete="postal-code"
						value={value.postcode}
						onChange={set('postcode')}
					/>
				</Field>
				<Field label={t('onboarding.city')} htmlFor="co-city" required error={errors.city}>
					<Input
						autoComplete="address-level2"
						value={value.city}
						onChange={set('city')}
					/>
				</Field>
			</div>
			<Field
				label={t('onboarding.street')}
				htmlFor="co-street"
				required
				error={errors.street}
			>
				<Input autoComplete="address-line1" value={value.street} onChange={set('street')} />
			</Field>

			<fieldset aria-describedby={errors.vatScheme ? 'co-vat-error' : undefined}>
				<legend className="text-sm font-medium text-fg">
					{t('onboarding.vatScheme')}
					<span className="ml-0.5 text-danger" aria-hidden="true">
						*
					</span>
				</legend>
				<div className="mt-2 space-y-2">
					{VAT_SCHEMES.map((s) => (
						<Choice
							key={s.value}
							name="co-vat"
							checked={value.vatScheme === s.value}
							onChange={() => onChange({ ...value, vatScheme: s.value })}
							label={t(s.label)}
							hint={t(s.hint)}
						/>
					))}
				</div>
				{errors.vatScheme && (
					<p id="co-vat-error" role="alert" className="mt-1 text-xs text-danger">
						{errors.vatScheme}
					</p>
				)}
			</fieldset>

			<fieldset aria-describedby={errors.incomeRegime ? 'co-income-error' : undefined}>
				<legend className="text-sm font-medium text-fg">
					{t('onboarding.incomeRegime')}
				</legend>
				<div className="mt-2 space-y-2">
					{INCOME_REGIMES.map((r) => (
						<Choice
							key={r.value}
							name="co-income"
							checked={value.incomeRegime === r.value}
							onChange={() => onChange({ ...value, incomeRegime: r.value })}
							label={t(r.label)}
							hint={t(r.hint)}
						/>
					))}
				</div>
				{errors.incomeRegime && (
					<p id="co-income-error" role="alert" className="mt-1 text-xs text-danger">
						{errors.incomeRegime}
					</p>
				)}
			</fieldset>

			{value.incomeRegime === 'ATALANY' && (
				<fieldset
					id="co-ratio"
					aria-describedby={errors.expenseRatioPct ? 'co-ratio-error' : undefined}
				>
					<legend className="text-sm font-medium text-fg">{t('onboarding.ratio')}</legend>
					<div className="mt-2 space-y-2">
						{EXPENSE_RATIOS.map((r) => (
							<Choice
								key={String(r.value)}
								name="co-ratio"
								checked={value.expenseRatioPct === r.value}
								onChange={() => onChange({ ...value, expenseRatioPct: r.value })}
								label={t(r.label)}
							/>
						))}
					</div>
					{errors.expenseRatioPct && (
						<p id="co-ratio-error" role="alert" className="mt-1 text-xs text-danger">
							{errors.expenseRatioPct}
						</p>
					)}
				</fieldset>
			)}

			{value.incomeRegime !== 'NONE' || value.vatScheme === 'ALANYI_MENTES' ? (
				<Field
					label={t('onboarding.regimeSince')}
					htmlFor="co-since"
					error={errors.regimeSince}
					hint={t('onboarding.regimeSince.hint')}
				>
					<Input type="date" value={value.regimeSince} onChange={set('regimeSince')} />
				</Field>
			) : null}

			<Field
				label={t('onboarding.bankAccount')}
				htmlFor="co-bank"
				error={errors.bankAccount}
				hint={t('common.optional')}
			>
				<Input
					inputMode="numeric"
					placeholder="12345678-12345678"
					value={value.bankAccount}
					onChange={(e) =>
						onChange({ ...value, bankAccount: maskBankAccount(e.target.value) })
					}
				/>
			</Field>
			<Field
				label={t('onboarding.bankName')}
				htmlFor="co-bank-name"
				error={errors.bankName}
				hint={t('common.optional')}
			>
				<Input value={value.bankName} onChange={set('bankName')} />
			</Field>

			<details className="rounded-md border border-line bg-surface-raised p-3">
				<summary className="cursor-pointer text-sm font-medium text-fg">
					{t('onboarding.moreTax')}
				</summary>
				<div className="mt-3 space-y-4">
					<Field
						label={t('onboarding.groupTaxNo')}
						htmlFor="co-group"
						error={errors.groupMemberTaxNo}
						hint={t('common.optional')}
					>
						<Input value={value.groupMemberTaxNo} onChange={set('groupMemberTaxNo')} />
					</Field>
					<Field
						label={t('onboarding.euVatId')}
						htmlFor="co-eu"
						error={errors.euVatId}
						hint={t('common.optional')}
					>
						<Input
							placeholder="HU12345678"
							value={value.euVatId}
							onChange={set('euVatId')}
						/>
					</Field>
				</div>
			</details>

			<details className="rounded-md border border-line bg-surface-raised p-3">
				<summary className="cursor-pointer text-sm font-medium text-fg">
					{t('onboarding.numbering')}
				</summary>
				<div className="mt-3">
					<Field
						label={t('onboarding.seriesCode')}
						htmlFor="co-series"
						required
						error={errors.seriesCode}
						hint={t('onboarding.seriesCode.hint')}
					>
						<Input
							readOnly={locked}
							value={value.seriesCode}
							onChange={set('seriesCode')}
						/>
					</Field>
				</div>
			</details>
		</div>
	)
}

// ---------------------------------------------------------------- NAV credentials

/**
 * The four NAV technical-user fields. Write-only: nothing is ever prefilled, because the
 * server never hands a secret back. Owns its own step-up retry.
 */
export function NavCredentialsForm({
	onDone,
	secondary
}: {
	onDone: (status: NavCredentialsStatus) => void
	/** The way out beside the "Connect and verify" primary — "Do it later" or "Cancel". */
	secondary: { label: string; onClick: () => void }
}) {
	const { t, tn, err, fields } = useT()
	const toast = useToast()
	const set = useSetNavCredentials()
	const [login, setLogin] = React.useState('')
	const [password, setPassword] = React.useState('')
	const [showPassword, setShowPassword] = React.useState(false)
	const [signKey, setSignKey] = React.useState('')
	const [exchangeKey, setExchangeKey] = React.useState('')
	const [errors, setErrors] = React.useState<Record<string, string>>({})
	const [banner, setBanner] = React.useState<string | null>(null)
	const [stepUp, setStepUp] = React.useState(false)

	async function submit() {
		setErrors({})
		setBanner(null)
		const missing: Record<string, string> = {}
		for (const [k, v] of Object.entries({
			login,
			techPassword: password,
			signKey,
			exchangeKey
		})) {
			if (v.trim() === '') missing[k] = t('common.required')
		}
		if (Object.keys(missing).length > 0) {
			setErrors(missing)
			return
		}
		try {
			const status = await set.mutateAsync({
				login,
				techPassword: password,
				signKey,
				exchangeKey
			})
			setStepUp(false)
			toast.success(
				status.unreported > 0
					? `${t('navConn.connected.toast')} ${tn('navConn.backlog', status.unreported)}`
					: t('navConn.connected.toast')
			)
			onDone(status)
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(true)
				return
			}
			setStepUp(false)
			if (e instanceof ServerError && e.errCode === 'E-NAV-CREDENTIALS-INVALID') {
				setErrors({ login: err(e), techPassword: err(e) })
				return
			}
			const f = fields(e)
			if (Object.keys(f).length > 0) setErrors(f)
			else setBanner(err(e))
		}
	}

	return (
		<>
			<form
				noValidate
				onSubmit={(e) => {
					e.preventDefault()
					void submit()
				}}
				className="space-y-4"
			>
				<details className="rounded-md border border-line bg-surface-raised p-3 text-sm">
					<summary className="cursor-pointer font-medium text-fg">
						{t('navConn.where')}
					</summary>
					<p className="mt-2 text-fg-muted">{t('navConn.where.body')}</p>
				</details>

				{banner && (
					<div className="space-y-2">
						<ErrorBanner message={banner} />
						<Button type="submit" variant="secondary" loading={set.isPending}>
							{t('common.retry')}
						</Button>
					</div>
				)}

				<Field label={t('navConn.login')} htmlFor="nav-login" required error={errors.login}>
					<Input
						autoComplete="off"
						value={login}
						onChange={(e) => setLogin(e.target.value)}
					/>
				</Field>
				<Field
					label={t('navConn.password')}
					htmlFor="nav-password"
					required
					error={errors.techPassword}
				>
					<Input
						type={showPassword ? 'text' : 'password'}
						autoComplete="off"
						value={password}
						onChange={(e) => setPassword(e.target.value)}
					/>
				</Field>
				<label className="flex items-center gap-2 text-sm text-fg-muted">
					<input
						type="checkbox"
						checked={showPassword}
						onChange={(e) => setShowPassword(e.target.checked)}
						className="h-4 w-4 accent-accent"
					/>
					{t('navConn.showPassword')}
				</label>
				<Field
					label={t('navConn.signKey')}
					htmlFor="nav-sign"
					required
					error={errors.signKey}
				>
					<Input
						autoComplete="off"
						value={signKey}
						onChange={(e) => setSignKey(e.target.value)}
					/>
				</Field>
				<Field
					label={t('navConn.exchangeKey')}
					htmlFor="nav-exchange"
					required
					error={errors.exchangeKey}
					hint={t('navConn.exchangeKey.count', { n: exchangeKey.trim().length })}
				>
					<Input
						autoComplete="off"
						maxLength={16}
						value={exchangeKey}
						onChange={(e) => setExchangeKey(e.target.value)}
					/>
				</Field>

				{set.isPending && (
					<p role="status" className="text-sm text-fg-muted">
						{t('navConn.checking')}
					</p>
				)}

				<div className="flex flex-wrap justify-end gap-2">
					<Button type="button" variant="secondary" onClick={secondary.onClick}>
						{secondary.label}
					</Button>
					<Button type="submit" loading={set.isPending}>
						{t('navConn.connect')}
					</Button>
				</div>
			</form>
			<StepUpDialog
				open={stepUp}
				onClose={() => setStepUp(false)}
				onAuthenticated={() => {
					setStepUp(false)
					void submit()
				}}
			/>
		</>
	)
}

// ---------------------------------------------------------------- /setup

type Step = 1 | 2

export function Setup() {
	const { t } = useT()
	const { me } = useAuth()
	const seller = useSeller()
	const navigate = useNavigate()
	const [searchParams] = useSearchParams()
	// `/setup?new=1` from inside a company: the org does not exist yet, whatever the active one is.
	const [newCompany, setNewCompany] = React.useState(() => searchParams.has('new'))
	// Held here, not in the form: creating the org switches the session, and a failed mint
	// after that must find the typed values still in place.
	const [company, setCompany] = React.useState<Company>(EMPTY_COMPANY)
	const [step, setStep] = React.useState<Step>(1)
	const [creating, setCreating] = React.useState(false)
	const heading = React.useRef<HTMLHeadingElement>(null)

	React.useEffect(() => {
		heading.current?.focus()
	}, [step])

	if (!me) return <PageSpinner />
	const kind = me.org?.kind
	if (kind === 'ROOT') return <Navigate to="/" replace />

	const shared = me.orgs.filter((o) => o.kind === 'SHARED')
	const inShared = kind === 'SHARED'
	if (inShared && seller.isPending) return <PageSpinner />
	const complete = inShared && !needsSetup(seller.error, seller.data)
	if (complete && step === 1 && !newCompany) return <Navigate to="/" replace />

	const done = () => navigate('/', { replace: true })

	return (
		<main className="min-h-screen bg-surface px-4 py-10 text-fg">
			<div className="mx-auto max-w-xl rounded-xl border border-line bg-surface-raised p-5">
				{!inShared && shared.length > 0 && !creating && !newCompany ? (
					<ChooseCompany orgs={shared} onCreate={() => setCreating(true)} />
				) : (
					<>
						<ol
							className="mb-6 flex items-center gap-3 text-sm"
							aria-label={t('onboarding.steps')}
						>
							<li
								aria-current={step === 1 ? 'step' : undefined}
								className={
									step === 1 ? 'font-semibold text-accent' : 'text-fg-muted'
								}
							>
								<span aria-hidden="true">{step === 1 ? '● ' : '✓ '}</span>
								{t('onboarding.step.company')}
							</li>
							<li aria-hidden="true" className="text-fg-muted">
								──
							</li>
							<li
								aria-current={step === 2 ? 'step' : undefined}
								className={
									step === 2 ? 'font-semibold text-accent' : 'text-fg-muted'
								}
							>
								<span aria-hidden="true">{step === 2 ? '● ' : '○ '}</span>
								{t('onboarding.step.nav')}
							</li>
						</ol>
						{step === 1 ? (
							<CompanyStep
								headingRef={heading}
								company={company}
								setCompany={setCompany}
								finish={inShared && !newCompany}
								onOrgCreated={() => setNewCompany(false)}
								onMinted={() => setStep(2)}
							/>
						) : (
							<section>
								<h1
									ref={heading}
									tabIndex={-1}
									className="text-xl font-semibold outline-none"
								>
									{t('navConn.title')}
								</h1>
								<p className="mt-1 text-sm text-fg-muted">{t('navConn.why')}</p>
								<div className="mt-6">
									<NavCredentialsForm
										onDone={done}
										secondary={{ label: t('navConn.later'), onClick: done }}
									/>
								</div>
							</section>
						)}
					</>
				)}
			</div>
		</main>
	)
}

function ChooseCompany({
	orgs,
	onCreate
}: {
	orgs: { uid: string; name: string }[]
	onCreate: () => void
}) {
	const { t, err } = useT()
	const { reload } = useAuth()
	const qc = useQueryClient()
	const [error, setError] = React.useState<string | null>(null)
	const [busy, setBusy] = React.useState<string | null>(null)

	async function pick(orgUid: string) {
		setBusy(orgUid)
		setError(null)
		try {
			await api.post('/api/auth/switch-org', { orgUid })
			qc.clear()
			await reload()
		} catch (e) {
			setError(err(e))
		} finally {
			setBusy(null)
		}
	}

	return (
		<section className="space-y-4">
			<h1 className="text-xl font-semibold">{t('onboarding.choose')}</h1>
			<ErrorBanner message={error} />
			<ul className="space-y-2">
				{orgs.map((o) => (
					<li key={o.uid}>
						<Button
							variant="secondary"
							className="w-full justify-start"
							loading={busy === o.uid}
							onClick={() => void pick(o.uid)}
						>
							{o.name}
						</Button>
					</li>
				))}
			</ul>
			<Button onClick={onCreate}>{t('onboarding.createAnother')}</Button>
		</section>
	)
}

function CompanyStep({
	headingRef,
	company,
	setCompany,
	finish,
	onOrgCreated,
	onMinted
}: {
	headingRef: React.RefObject<HTMLHeadingElement | null>
	company: Company
	setCompany: (c: Company) => void
	/** The org exists already — a resumed or inherited-seller setup: submit only mints. */
	finish: boolean
	/** The new org is active: a retry after a failed mint must not create a second one. */
	onOrgCreated: () => void
	onMinted: () => void
}) {
	const { t, err, fields } = useT()
	const { me, reload } = useAuth()
	const qc = useQueryClient()
	const mint = useCreateSeller()
	const [errors, setErrors] = React.useState<Record<string, string>>({})
	const [banner, setBanner] = React.useState<string | null>(null)
	const [busy, setBusy] = React.useState(false)

	async function submit() {
		setBanner(null)
		const local = companyErrors(company)
		if (Object.keys(local).length > 0) {
			setErrors(Object.fromEntries(Object.entries(local).map(([k, v]) => [k, t(v)])))
			return
		}
		setErrors({})
		setBusy(true)
		try {
			if (!finish) {
				// `Auth::create_org` parents the new org under an active SHARED one; from the
				// PERSONAL org every account owns, it is top-level and independent.
				const personal = me?.orgs.find((o) => o.kind === 'PERSONAL')
				if (me?.org?.kind === 'SHARED' && personal) {
					await api.post('/api/auth/switch-org', { orgUid: personal.uid })
					qc.clear()
					await reload()
				}
				const org = await api.post<{ uid: string }>('/api/orgs', {
					name: company.name.trim()
				})
				await api.post('/api/auth/switch-org', { orgUid: org.uid })
				qc.clear()
				await reload()
				onOrgCreated()
			}
			await mint.mutateAsync({
				...sellerBody(company),
				seriesCode: company.seriesCode.trim()
			})
			onMinted()
		} catch (e) {
			const f = serverFieldErrors(e, fields, err)
			if (Object.keys(f).length > 0) setErrors(f)
			else setBanner(err(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<form
			noValidate
			onSubmit={(e) => {
				e.preventDefault()
				void submit()
			}}
		>
			<h1 ref={headingRef} tabIndex={-1} className="text-xl font-semibold outline-none">
				{t('onboarding.title')}
			</h1>
			<p className="mt-1 text-sm text-fg-muted">{t('onboarding.subtitle')}</p>
			<div className="mt-4">
				<ErrorBanner message={banner} />
			</div>
			<div className="mt-6">
				<SellerFields value={company} onChange={setCompany} errors={errors} />
			</div>
			<Button type="submit" className="mt-6" loading={busy}>
				{t('onboarding.submit')}
			</Button>
		</form>
	)
}

// vim: ts=4
