import { useQueryClient } from '@tanstack/react-query'
import * as React from 'react'
import { Link, NavLink, Outlet, useLocation } from 'react-router-dom'

import { api, useAuth, useNavCredentials, useSeller } from '@saas-framework/client'

import { Select } from '~/components/ui'
import { useToast } from '~/components/Toast'
import { type Locale, LOCALE_NAMES, useT } from '~/i18n'
import { useTheme } from '~/theme'

const TABS = [
	{ to: '/', key: 'nav.dashboard', end: true },
	{ to: '/invoices', key: 'nav.invoices', end: false },
	{ to: '/partners', key: 'nav.partners', end: false },
	{ to: '/services', key: 'nav.services', end: false },
	{ to: '/projects', key: 'nav.projects', end: false },
	{ to: '/settings/company', key: 'nav.company', end: false }
] as const

export function AppShell() {
	const { t } = useT()
	const { pathname } = useLocation()
	const main = React.useRef<HTMLElement>(null)
	const [drawer, setDrawer] = React.useState(false)
	const seller = useSeller()
	const { me } = useAuth()
	const closed = Boolean(seller.data?.closedAt)

	// The browser moves focus on a real navigation but not on a client-side route change, so
	// without this a keyboard or screen-reader user stays on the tab they just left and hears
	// nothing about the new view.
	React.useEffect(() => {
		main.current?.focus()
	}, [pathname])

	// A tap on a nav link inside the drawer changes the route but not this state, so the
	// panel would stay open over the page it just navigated to.
	React.useEffect(() => {
		setDrawer(false)
	}, [pathname])

	const brand = seller.data?.name ?? t('app.title')
	const nav = (
		<nav className="flex flex-col gap-1" aria-label={t('nav.main')}>
			{TABS.map((tab) => (
				<NavLink
					key={tab.to}
					to={tab.to}
					end={tab.end}
					className={({ isActive }) =>
						`flex min-h-[44px] items-center rounded-lg px-3 text-sm ${
							isActive
								? 'bg-surface-sunken font-medium text-accent shadow-[inset_3px_0_0_var(--accent)]'
								: 'text-fg-muted hover:bg-surface-sunken'
						}`
					}
				>
					{t(tab.key)}
				</NavLink>
			))}
		</nav>
	)

	return (
		<div className="min-h-screen bg-surface text-fg">
			<a
				href="#main"
				className="sr-only focus:not-sr-only focus:absolute focus:left-4 focus:top-4 focus:z-50 focus:rounded-lg focus:bg-surface-raised focus:px-4 focus:py-2 focus:text-sm focus:font-medium focus:text-accent"
			>
				{t('shell.skip')}
			</a>

			<header className="sticky top-0 z-30 border-b border-line bg-surface-raised">
				<div className="mx-auto flex max-w-7xl items-center gap-3 px-4 py-2">
					<button
						type="button"
						onClick={() => setDrawer(true)}
						aria-label={t('nav.open')}
						className="flex min-h-[44px] min-w-[44px] items-center justify-center rounded-lg text-fg-muted hover:bg-surface-sunken lg:hidden"
					>
						<span aria-hidden="true">☰</span>
					</button>
					<Link to="/" className="truncate text-sm font-semibold text-fg">
						{brand}
					</Link>
					<div className="flex flex-1 flex-wrap items-center justify-end gap-2">
						{me?.org?.kind === 'SHARED' && (
							<Link
								to="/setup?new=1"
								className="inline-flex min-h-[44px] items-center rounded-md px-3 text-sm font-medium text-fg-muted hover:bg-surface-sunken"
							>
								{t('shell.newCompany')}
							</Link>
						)}
						<OrgSwitcher />
						<ThemeToggle />
						<LanguageSwitcher />
						<UserMenu />
					</div>
				</div>
			</header>

			{/* Nothing new will be issued while read-only, so the NAV nag gives way. */}
			{closed ? <ReadOnlyBand /> : <NavBand />}

			<Drawer open={drawer} onClose={() => setDrawer(false)} title={t('nav.main')}>
				{nav}
			</Drawer>

			<div className="mx-auto flex max-w-7xl gap-8 px-4 py-6">
				<aside className="hidden w-52 shrink-0 lg:block">{nav}</aside>
				<main id="main" ref={main} tabIndex={-1} className="min-w-0 flex-1 outline-none">
					<Outlet />
				</main>
			</div>
		</div>
	)
}

function ReadOnlyBand() {
	const { t } = useT()
	return (
		<div role="status" className="border-b border-warning bg-surface-raised">
			<p className="mx-auto max-w-7xl px-4 py-2 text-sm text-fg">{t('shell.readOnly')}</p>
		</div>
	)
}

/**
 * Not dismissible: issued invoices pile up unreported until NAV is connected. Hidden when the
 * status cannot be read — a Member gets a 403, and the band is an Admin's call to act.
 */
function NavBand() {
	const { t, tn } = useT()
	const status = useNavCredentials()
	if (status.data?.connected !== false) return null
	const n = status.data.unreported
	return (
		<div role="status" className="border-b border-warning bg-surface-raised">
			<p className="mx-auto flex max-w-7xl flex-wrap items-center gap-x-2 px-4 py-2 text-sm text-fg">
				<span aria-hidden="true" className="text-warning">
					⚠
				</span>
				<span>{n > 0 ? tn('navBand.waiting', n) : t('navBand.notConnected')}</span>
				<Link to="/settings/company#nav" className="font-medium text-accent underline">
					{t('navBand.connect')}
				</Link>
			</p>
		</div>
	)
}

/**
 * The off-canvas nav below `lg`. A real `<dialog>`, so the focus trap, Esc and `aria-modal`
 * are the platform's; the backdrop click is the one thing `<dialog>` does not give.
 */
function Drawer({
	open,
	onClose,
	title,
	children
}: {
	open: boolean
	onClose: () => void
	title: string
	children: React.ReactNode
}) {
	const ref = React.useRef<HTMLDialogElement>(null)
	const { t } = useT()

	React.useEffect(() => {
		const d = ref.current
		if (!d) return
		if (open && !d.open) d.showModal()
		if (!open && d.open) d.close()
	}, [open])

	return (
		// biome-ignore lint/a11y/useKeyWithClickEvents: the click only dismisses the backdrop; <dialog> closes on Escape through onCancel.
		<dialog
			ref={ref}
			onClose={onClose}
			onCancel={onClose}
			onClick={(e) => {
				if (e.target === ref.current) onClose()
			}}
			aria-label={title}
			className="m-0 h-full max-h-none w-72 max-w-[85vw] border-r border-line bg-surface-raised p-0 text-fg backdrop:bg-black/50"
		>
			<div className="flex items-center justify-between border-b border-line px-4 py-3">
				<span className="text-sm font-semibold">{title}</span>
				<button
					type="button"
					onClick={onClose}
					aria-label={t('nav.close')}
					className="flex min-h-[44px] min-w-[44px] items-center justify-center rounded-lg text-fg-muted hover:bg-surface-sunken"
				>
					<span aria-hidden="true">×</span>
				</button>
			</div>
			<div className="p-3">{children}</div>
		</dialog>
	)
}

/** Rendered only on a multi-org account: a select with one option is a control that lies. */
function OrgSwitcher() {
	const { me, reload } = useAuth()
	const { t, err } = useT()
	const toast = useToast()
	const qc = useQueryClient()
	const [busy, setBusy] = React.useState(false)

	if (!me || me.orgs.length < 2) return null

	async function switchTo(orgUid: string) {
		const picked = me?.orgs.find((o) => o.uid === orgUid)
		setBusy(true)
		try {
			await api.post('/api/auth/switch-org', { orgUid })
			// Nothing in the cache is keyed by org, so every cached row belongs to the org the
			// session just left.
			qc.clear()
			await reload()
			if (picked) toast.success(t('shell.orgSwitched', { name: picked.name }))
		} catch (e) {
			toast.error(err(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<div className="flex items-center gap-2">
			<Select
				aria-label={t('shell.org')}
				className="min-h-[40px] w-auto py-1 text-xs"
				disabled={busy}
				value={me.org?.uid ?? ''}
				onChange={(e) => void switchTo(e.target.value)}
			>
				{me.orgs.map((o) => (
					<option key={o.uid} value={o.uid}>
						{o.name}
					</option>
				))}
			</Select>
		</div>
	)
}

function ThemeToggle() {
	const { t } = useT()
	const [theme, setTheme] = useTheme()
	return (
		<div className="flex items-center gap-2">
			<Select
				aria-label={t('shell.theme')}
				className="min-h-[40px] w-auto py-1 text-xs"
				value={theme}
				onChange={(e) => setTheme(e.target.value as 'system' | 'light' | 'dark')}
			>
				<option value="system">{t('shell.theme.system')}</option>
				<option value="light">{t('shell.theme.light')}</option>
				<option value="dark">{t('shell.theme.dark')}</option>
			</Select>
		</div>
	)
}

function LanguageSwitcher() {
	const { t, locale, setLocale } = useT()
	return (
		<div className="flex items-center gap-2">
			<Select
				aria-label={t('shell.language')}
				className="min-h-[40px] w-auto py-1 text-xs"
				value={locale}
				onChange={(e) => setLocale(e.target.value as Locale)}
			>
				{/* Endonyms, never flags: a flag is a country and these are languages. */}
				{(Object.keys(LOCALE_NAMES) as Locale[]).map((l) => (
					<option key={l} value={l}>
						{LOCALE_NAMES[l]}
					</option>
				))}
			</Select>
		</div>
	)
}

/**
 * A disclosure menu, with the two things a native `<details>` does not do: Esc closes it and
 * so does a click outside. Focus goes back to the button on Esc, or the keyboard user is
 * left at the top of the document.
 */
function UserMenu() {
	const { me, logout } = useAuth()
	const { t } = useT()
	const [open, setOpen] = React.useState(false)
	const box = React.useRef<HTMLDivElement>(null)
	const btn = React.useRef<HTMLButtonElement>(null)

	React.useEffect(() => {
		if (!open) return
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') {
				setOpen(false)
				btn.current?.focus()
			}
		}
		const onDown = (e: MouseEvent) => {
			if (!box.current?.contains(e.target as Node)) setOpen(false)
		}
		document.addEventListener('keydown', onKey)
		document.addEventListener('mousedown', onDown)
		return () => {
			document.removeEventListener('keydown', onKey)
			document.removeEventListener('mousedown', onDown)
		}
	}, [open])

	return (
		<div ref={box} className="relative">
			<button
				ref={btn}
				type="button"
				aria-haspopup="menu"
				aria-expanded={open}
				aria-label={t('shell.userMenu')}
				onClick={() => setOpen((o) => !o)}
				className="flex min-h-[44px] max-w-[14rem] items-center gap-2 rounded-lg px-3 text-sm text-fg-muted hover:bg-surface-sunken"
			>
				<span className="truncate">{me?.account.email}</span>
				<span aria-hidden="true">▾</span>
			</button>
			{open && (
				<div className="absolute right-0 z-40 mt-1 w-48 rounded-lg border border-line bg-surface-raised py-1 shadow-lg">
					<Link
						to="/account"
						onClick={() => setOpen(false)}
						className="flex min-h-[44px] items-center px-3 text-sm text-fg hover:bg-surface-sunken"
					>
						{t('nav.account')}
					</Link>
					<Link
						to="/account/api-keys"
						onClick={() => setOpen(false)}
						className="flex min-h-[44px] items-center px-3 text-sm text-fg hover:bg-surface-sunken"
					>
						{t('nav.apiKeys')}
					</Link>
					<button
						type="button"
						onClick={() => void logout()}
						className="flex min-h-[44px] w-full items-center px-3 text-left text-sm text-fg hover:bg-surface-sunken"
					>
						{t('shell.signOut')}
					</button>
				</div>
			)}
		</div>
	)
}

// vim: ts=4
