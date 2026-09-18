// Every server call the application screens make, with the query keys and their
// invalidations written once. Nothing here decides anything: the rules live in the
// backend service handles, and a hook is a URL plus what it invalidates.

import type { QueryClient } from '@tanstack/react-query'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api } from '~/api/client'
import type {
	BillingParty,
	Booking,
	BookRequest,
	InvoiceView,
	LegalKind,
	MoneyWire,
	NavSubmission,
	Page,
	ServiceView
} from '~/api/types'

/**
 * `GET /api/consents` answers an `Items<T>` — a bare `items` array, no cursor
 * (`crates/saas-auth/src/consent.rs`). Declared here rather than in `api/types.ts`:
 * the Account screen is the only reader.
 */
export interface Consent {
	kind: LegalKind
	docVersion: string
	docSha256: string
	granted: boolean
	at: string
	withdrawnAt: string | null
	tenantUid: string | null
}

/**
 * `invoice(uid)` sits *under* `invoices`, so invalidating the list also refetches every
 * open detail and its NAV state — one invalidation after any invoice mutation.
 */
export const keys = {
	services: ['services'] as const,
	bookings: ['bookings'] as const,
	parties: ['parties'] as const,
	invoices: ['invoices'] as const,
	invoice: (uid: string) => ['invoices', uid] as const,
	nav: (uid: string) => ['invoices', uid, 'nav'] as const,
	consents: ['consents'] as const
}

function invalidateInvoices(qc: QueryClient) {
	qc.invalidateQueries({ queryKey: keys.invoices })
	qc.invalidateQueries({ queryKey: keys.bookings })
}

export function useServices() {
	return useQuery({
		queryKey: keys.services,
		queryFn: ({ signal }) => api.get<Page<ServiceView>>('/api/services', signal)
	})
}

export function useBookings() {
	return useQuery({
		queryKey: keys.bookings,
		queryFn: ({ signal }) => api.get<Page<Booking>>('/api/bookings', signal)
	})
}

export function useParties() {
	return useQuery({
		queryKey: keys.parties,
		queryFn: ({ signal }) => api.get<Page<BillingParty>>('/api/billing-parties', signal)
	})
}

/**
 * The one paged list: an account accumulates invoices forever, and page one presented as the
 * whole list is a customer who cannot see an invoice they were charged for. The other lists
 * stay single-page and say so when `nextCursor` is non-null.
 */
export function useInvoices() {
	return useInfiniteQuery({
		queryKey: keys.invoices,
		queryFn: ({ pageParam, signal }) =>
			api.get<Page<InvoiceView>>(
				pageParam
					? `/api/invoices?cursor=${encodeURIComponent(pageParam)}`
					: '/api/invoices',
				signal
			),
		initialPageParam: '',
		getNextPageParam: (last) => last.nextCursor ?? undefined
	})
}

/** The single read: `lines` and `vatSummary` are omitted from the listing. */
export function useInvoice(uid: string) {
	return useQuery({
		queryKey: keys.invoice(uid),
		queryFn: ({ signal }) => api.get<InvoiceView>(`/api/invoices/${uid}`, signal),
		enabled: uid !== ''
	})
}

/** 204 — nothing filed — arrives as `null`, which is the "not reported" state. */
export function useNavSubmission(uid: string, enabled: boolean) {
	return useQuery({
		queryKey: keys.nav(uid),
		queryFn: ({ signal }) => api.get<NavSubmission | null>(`/api/invoices/${uid}/nav`, signal),
		enabled: enabled && uid !== ''
	})
}

export function useConsents() {
	return useQuery({
		queryKey: keys.consents,
		queryFn: ({ signal }) => api.get<{ items: Consent[] }>('/api/consents', signal)
	})
}

export function useBook() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: BookRequest) => api.post<Booking>('/api/bookings', body),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.bookings })
	})
}

/** `null` means 204: there was nothing unbilled to check out. */
export function useCheckout() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: () => api.post<InvoiceView | null>('/api/bookings/checkout'),
		onSuccess: () => invalidateInvoices(qc)
	})
}

export type InvoiceAction = 'confirm' | 'payment' | 'cancel'

/** The example's own three lifecycle routes — `tenant_invoices()` is deliberately unmounted. */
export function useInvoiceAction(uid: string) {
	const qc = useQueryClient()
	return useMutation({
		// `payment` carries the gross the screen is showing: the server refuses a mismatch,
		// because PAID is one-way and a wrong-amount invoice can then never be stornoed.
		mutationFn: ({
			action,
			reason,
			amount
		}: {
			action: InvoiceAction
			reason?: string
			amount?: MoneyWire
		}) =>
			api.post<InvoiceView>(
				`/api/invoices/${uid}/${action}`,
				action === 'cancel'
					? { reason: reason ?? '' }
					: action === 'payment'
						? amount
						: undefined
			),
		onSuccess: () => invalidateInvoices(qc)
	})
}

export function useSaveParty(uid: string | null) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: Record<string, unknown>) =>
			uid
				? api.patch<BillingParty>(`/api/billing-parties/${uid}`, body)
				: api.post<BillingParty>('/api/billing-parties', body),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.parties })
	})
}

export function useRecordConsent() {
	const qc = useQueryClient()
	return useMutation({
		// No `tenantUid`: TOS and PRIVACY are account-wide, and a tenant-scoped grant satisfies
		// the gate for nothing while `GET /api/consents` still reports it (`E-AUTH-CONSENT-SCOPE`).
		mutationFn: (d: { kind: LegalKind; version: string; docSha256: string }) =>
			api.post<void>('/api/consents', d),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.consents })
	})
}

export function useWithdrawConsent() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (kind: LegalKind) => api.delete<void>(`/api/consents/${kind}`),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.consents })
	})
}

/** Re-auth for the step-up gate. A code with no password is refused server-side. */
export function useStepUp() {
	return useMutation({
		mutationFn: (password: string) => api.post<void>('/api/auth/step-up', { password })
	})
}

/** Step-up gated and it wants the account's own email back in the body. */
export function useDeleteAccount() {
	return useMutation({
		mutationFn: (confirmEmail: string) =>
			api.post<{
				status: string
				retainedUntil: string | null
				retainedBecause: string | null
			}>('/api/account/delete', { confirmEmail })
	})
}

// vim: ts=4
