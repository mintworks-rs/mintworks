import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom'

import './index.css'

import { AuthProvider, ServerError } from '@mintworks/client'
import { ProtectedRoute } from '~/auth/ProtectedRoute'
import { ToastProvider } from '~/components/Toast'
import { AppShell } from '~/layout/AppShell'
import { Account } from '~/pages/Account'
import { ApiKeys } from '~/pages/ApiKeys'
import { Activate } from '~/pages/auth/Activate'
import { CheckEmail } from '~/pages/auth/CheckEmail'
import { Login } from '~/pages/auth/Login'
import { Register } from '~/pages/auth/Register'
import { Reset } from '~/pages/auth/Reset'
import { ResetRequest } from '~/pages/auth/ResetRequest'
import { Billing } from '~/pages/Billing'
import { Book } from '~/pages/Book'
import { InvoiceDetail } from '~/pages/InvoiceDetail'
import { Invoices } from '~/pages/Invoices'
import { QrApprove } from '~/pages/QrApprove'

const queryClient = new QueryClient({
	defaultOptions: {
		queries: {
			// A 401 that survived `client.ts`'s one refresh-and-retry is a dead session, not a
			// blip: retrying it spends a second refresh per query on the way to /login.
			retry: (n, err) => !(err instanceof ServerError && err.httpStatus === 401) && n < 1,
			refetchOnWindowFocus: false
		}
	}
})

function App() {
	return (
		<QueryClientProvider client={queryClient}>
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
							{/* Outside the shell: the phone scanning the code is signed in as itself, and
							    the session it produces belongs to the desktop that is polling for it. */}
							<Route path="/qr/:sessionId" element={<QrApprove />} />
							<Route
								path="/"
								element={
									<ProtectedRoute>
										<AppShell />
									</ProtectedRoute>
								}
							>
								<Route index element={<Book />} />
								<Route path="invoices" element={<Invoices />} />
								<Route path="invoices/:uid" element={<InvoiceDetail />} />
								<Route path="billing" element={<Billing />} />
								<Route path="account" element={<Account />} />
								<Route path="account/api-keys" element={<ApiKeys />} />
							</Route>
							<Route path="*" element={<Navigate to="/" replace />} />
						</Routes>
					</AuthProvider>
				</ToastProvider>
			</BrowserRouter>
		</QueryClientProvider>
	)
}

const root = createRoot(document.getElementById('app')!)
root.render(<App />)

// vim: ts=4
