import { useEffect, useRef } from 'react'
import { Link, NavLink, Outlet, useLocation } from 'react-router-dom'

import { useAuth } from '~/auth/AuthContext'

const TABS = [
	{ to: '/', label: 'Book', end: true },
	{ to: '/invoices', label: 'Invoices', end: false },
	{ to: '/billing', label: 'Billing', end: false },
	{ to: '/account', label: 'Account', end: false }
]

export function AppShell() {
	const { me, logout } = useAuth()
	const { pathname } = useLocation()
	const main = useRef<HTMLElement>(null)

	// The browser moves focus on a real navigation but not on a client-side route
	// change, so without this a keyboard or screen-reader user stays on the tab they
	// just left and hears nothing about the new view.
	useEffect(() => {
		main.current?.focus()
	}, [pathname])

	return (
		<div className="min-h-screen bg-slate-50">
			<a
				href="#main"
				className="sr-only focus:not-sr-only focus:absolute focus:left-4 focus:top-4 focus:z-50 focus:rounded-lg focus:bg-white focus:px-4 focus:py-2 focus:text-sm focus:font-medium focus:text-brand-700"
			>
				Skip to content
			</a>

			<header className="border-b border-slate-200 bg-white">
				<div className="mx-auto flex max-w-5xl items-center gap-6 px-4">
					<Link to="/" className="py-4 text-sm font-semibold text-slate-900">
						Példa Szolgáltató
					</Link>

					<nav className="flex flex-1 gap-1" aria-label="Main">
						{TABS.map((t) => (
							<NavLink
								key={t.to}
								to={t.to}
								end={t.end}
								className={({ isActive }) =>
									`flex min-h-[44px] items-center rounded-lg px-3 text-sm ${
										isActive
											? 'bg-brand-50 font-medium text-brand-700'
											: 'text-slate-600 hover:bg-slate-100'
									}`
								}
							>
								{t.label}
							</NavLink>
						))}
					</nav>

					{/* No disclosure menu: Account is already a tab, and a native <details>
					    closes on neither Esc nor an outside click. */}
					<span className="hidden text-sm text-slate-500 sm:block">
						{me?.account.email}
					</span>
					<button
						type="button"
						onClick={() => void logout()}
						className="flex min-h-[44px] items-center rounded-lg px-3 text-sm text-slate-600 hover:bg-slate-100"
					>
						Sign out
					</button>
				</div>
			</header>

			<main
				id="main"
				ref={main}
				tabIndex={-1}
				className="mx-auto max-w-5xl px-4 py-8 outline-none"
			>
				<Outlet />
			</main>
		</div>
	)
}

// vim: ts=4
