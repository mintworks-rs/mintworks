import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom'

import './index.css'

import { AuthProvider, ServerError } from '@saas-framework/client'
import { ProtectedRoute } from '~/auth/ProtectedRoute'
import { ToastProvider } from '~/components/Toast'
import { I18nProvider } from '~/i18n'
import { AppShell } from '~/layout/AppShell'
import { Account } from '~/pages/Account'
import { ApiKeys } from '~/pages/ApiKeys'
import { Activate } from '~/pages/auth/Activate'
import { CompanySettings } from '~/pages/CompanySettings'
import { Dashboard } from '~/pages/Dashboard'
import { InvoiceComposer } from '~/pages/InvoiceComposer'
import { InvoiceDetail } from '~/pages/InvoiceDetail'
import { Invoices } from '~/pages/Invoices'
import { CompanyGate, Setup } from '~/pages/Onboarding'
import { CheckEmail } from '~/pages/auth/CheckEmail'
import { Login } from '~/pages/auth/Login'
import { Register } from '~/pages/auth/Register'
import { Reset } from '~/pages/auth/Reset'
import { ResetRequest } from '~/pages/auth/ResetRequest'
import { Partners } from '~/pages/Partners'
import { ProjectDetail } from '~/pages/ProjectDetail'
import { Projects } from '~/pages/Projects'
import { QrApprove } from '~/pages/QrApprove'
import { Services } from '~/pages/Services'

const queryClient = new QueryClient({
	defaultOptions: {
		queries: {
			// A 401 that survived `http.ts`'s one refresh-and-retry is a dead session, not a
			// blip: retrying it spends a second refresh per query on the way to /login.
			retry: (n, err) => !(err instanceof ServerError && err.httpStatus === 401) && n < 1,
			refetchOnWindowFocus: false
		}
	}
})

// `AuthProvider` calls `useQueryClient`, `ToastProvider` and every screen call `useT`, and
// `ProtectedRoute` renders a `<Navigate>`, so this nesting is the one all three allow.
function App() {
	return (
		<QueryClientProvider client={queryClient}>
			<I18nProvider>
				<BrowserRouter>
					<ToastProvider>
						<AuthProvider>
							<Routes>
								<Route path="/login" element={<Login />} />
								<Route path="/register" element={<Register />} />
								<Route path="/register/check-email" element={<CheckEmail />} />
								<Route path="/activate" element={<Activate />} />
								<Route path="/password/reset-request" element={<ResetRequest />} />
								<Route path="/password/reset" element={<Reset />} />
								{/* Outside the shell: the phone scanning the code is signed in as
								    itself, and the session it produces belongs to the desktop
								    that is polling for it. */}
								<Route path="/qr/:sessionId" element={<QrApprove />} />
								{/* A full page, outside the shell and the gate that sends people here. */}
								<Route
									path="/setup"
									element={
										<ProtectedRoute>
											<Setup />
										</ProtectedRoute>
									}
								/>
								<Route
									path="/"
									element={
										<ProtectedRoute>
											<CompanyGate>
												<AppShell />
											</CompanyGate>
										</ProtectedRoute>
									}
								>
									<Route index element={<Dashboard />} />
									<Route path="invoices" element={<Invoices />} />
									{/* `/new` before `/:uid`: a static segment and a parameter at
									    the same depth, and the parameter would swallow it. */}
									<Route path="invoices/new" element={<InvoiceComposer />} />
									<Route path="invoices/:uid" element={<InvoiceDetail />} />
									<Route
										path="invoices/:uid/edit"
										element={<InvoiceComposer />}
									/>
									<Route path="partners" element={<Partners />} />
									<Route path="services" element={<Services />} />
									<Route path="projects" element={<Projects />} />
									<Route path="projects/:uid" element={<ProjectDetail />} />
									<Route path="settings/company" element={<CompanySettings />} />
									<Route path="account" element={<Account />} />
									<Route path="account/api-keys" element={<ApiKeys />} />
								</Route>
								<Route path="*" element={<Navigate to="/" replace />} />
							</Routes>
						</AuthProvider>
					</ToastProvider>
				</BrowserRouter>
			</I18nProvider>
		</QueryClientProvider>
	)
}

const root = createRoot(document.getElementById('app')!)
root.render(<App />)

// vim: ts=4
