import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom'

import './index.css'

import { AuthProvider, ServerError } from '@saas-framework/client'
import { ProtectedRoute } from '~/auth/ProtectedRoute'
import { Research } from '~/pages/Research'
import { Activate } from '~/pages/auth/Activate'
import { CheckEmail } from '~/pages/auth/CheckEmail'
import { Login } from '~/pages/auth/Login'
import { Register } from '~/pages/auth/Register'

const queryClient = new QueryClient({
	defaultOptions: {
		queries: {
			// A 401 that survived `http.ts`'s one refresh-and-retry is a dead session.
			retry: (n, err) => !(err instanceof ServerError && err.httpStatus === 401) && n < 1,
			refetchOnWindowFocus: false
		}
	}
})

// No `/consent` route: `ProtectedRoute`'s `consentGate` renders `ConsentRequired` in place.
function App() {
	return (
		<QueryClientProvider client={queryClient}>
			<BrowserRouter>
				<AuthProvider>
					<Routes>
						<Route path="/login" element={<Login />} />
						<Route path="/register" element={<Register />} />
						<Route path="/check-email" element={<CheckEmail />} />
						<Route path="/activate" element={<Activate />} />
						<Route
							path="/"
							element={
								<ProtectedRoute>
									<Research />
								</ProtectedRoute>
							}
						/>
						<Route
							path="/t/:thread"
							element={
								<ProtectedRoute>
									<Research />
								</ProtectedRoute>
							}
						/>
						<Route path="*" element={<Navigate to="/" replace />} />
					</Routes>
				</AuthProvider>
			</BrowserRouter>
		</QueryClientProvider>
	)
}

const root = createRoot(document.getElementById('app')!)
root.render(<App />)

// vim: ts=4
