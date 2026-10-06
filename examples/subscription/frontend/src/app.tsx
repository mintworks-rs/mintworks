// SPDX-License-Identifier: MIT-0
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom'

import './index.css'

import { AuthProvider, ServerError } from '@mintworks/client'
import { ProtectedRoute } from '~/auth/ProtectedRoute'
import { ToastProvider } from '~/components/Toast'
import { AppShell } from '~/layout/AppShell'
import { Account } from '~/pages/Account'
import { Activate } from '~/pages/auth/Activate'
import { CheckEmail } from '~/pages/auth/CheckEmail'
import { Login } from '~/pages/auth/Login'
import { Register } from '~/pages/auth/Register'
import { Reset } from '~/pages/auth/Reset'
import { ResetRequest } from '~/pages/auth/ResetRequest'
import { Generate } from '~/pages/Generate'
import { Invites } from '~/pages/Invites'
import { Pricing } from '~/pages/Pricing'
import { QrApprove } from '~/pages/QrApprove'
import { RefLanding } from '~/pages/RefLanding'

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
							<Route path="/qr/:sessionId" element={<QrApprove />} />
							<Route path="/r/:code" element={<RefLanding />} />
							<Route
								path="/"
								element={
									<ProtectedRoute>
										<AppShell />
									</ProtectedRoute>
								}
							>
								<Route index element={<Pricing />} />
								<Route path="account" element={<Account />} />
								<Route path="invites" element={<Invites />} />
								<Route path="generate" element={<Generate />} />
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
