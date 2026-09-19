// Every server call the application screens make, with the query keys and their
// invalidations written once. Nothing here decides anything: the rules live in the
// backend service handles, and a hook is a URL plus what it invalidates.

import { useEffect, useRef } from 'react'

import type { QueryClient } from '@tanstack/react-query'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api } from '~/api/client'
import type {
	BillingParty,
	Booking,
	BookRequest,
	CheckoutRequest,
	InvoiceView,
	LegalKind,
	NavSubmission,
	Page,
	PaymentState,
	PaymentView,
	ProviderView,
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
	payments: (uid: string) => ['invoices', uid, 'payments'] as const,
	providers: ['payment-providers'] as const,
	consents: ['consents'] as const
}

/** The four live states. Anything else is terminal and the page stops polling. */
const LIVE: PaymentState[] = ['PENDING', 'AWAITING_USER', 'RESERVED', 'AUTHORIZED']

// `payments(uid)` and `invoice(uid)` both sit under `invoices`, so this refetches them too.
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

/**
 * An empty list means card is simply not offered — the offline demo, with no `SAAS_PAYMENT_BARION_POS_KEY`
 * set, which is what `main.rs` gates registering the gateway on.
 */
export function useProviders() {
	return useQuery({
		queryKey: keys.providers,
		queryFn: ({ signal }) => api.get<Page<ProviderView>>('/api/payment-providers', signal)
	})
}

/**
 * Every payment opened against one invoice. Polled while one is live, because the gateway
 * settles out of band through its webhook and the return URL carries no state.
 *
 * ponytail: a fixed 2 s x 30 interval, not a backoff and not a push. `app.tsx` turns
 * `refetchOnWindowFocus` off globally, so the interval is the only thing that would notice;
 * SSE or a websocket is the upgrade if a demo ever needs sub-second feedback.
 */
export function useInvoicePayments(uid: string) {
	const qc = useQueryClient()
	const query = useQuery({
		queryKey: keys.payments(uid),
		queryFn: ({ signal }) =>
			api.get<Page<PaymentView>>(`/api/invoices/${uid}/payments`, signal),
		enabled: uid !== '',
		refetchInterval: (q) =>
			q.state.data?.items.some((p) => LIVE.includes(p.status)) && q.state.dataUpdateCount < 30
				? 2000
				: false
	})

	// The poll is what settles the invoice server-side, but a poll is not an invalidation: without
	// this the detail keeps serving the cached DRAFT until a manual reload.
	const seen = useRef<string | undefined>(undefined)
	const statuses = query.data?.items.map((p) => p.status).join(',')
	useEffect(() => {
		if (statuses === undefined) return
		const previous = seen.current
		seen.current = statuses
		// `keys.invoice(uid)` is a *prefix* of `keys.payments(uid)`, so a non-exact invalidation
		// would refetch this very query and fire a second gateway round trip per change. The
		// first sample counts: on the return from the gateway it is the response that issued the
		// invoice, and the `useInvoice` GET racing beside it had already answered PENDING.
		if (previous !== statuses)
			qc.invalidateQueries({ queryKey: keys.invoice(uid), exact: true })
	}, [statuses, uid, qc])

	return query
}

/** A fresh attempt sends no `requestId`: the server mints one, so this never collides with a
 *  payment already open under a spent key. */
export function useStartPayment(uid: string) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (provider: string) =>
			api.post<{ payment: PaymentView; redirectUrl: string | null }>(
				`/api/invoices/${uid}/pay`,
				{ provider, returnUrl: `${location.origin}/invoices/${uid}` }
			),
		onSuccess: () => invalidateInvoices(qc)
	})
}

/** "Pay another way": restamps the draft as a bank transfer and issues it. */
export function usePayByTransfer(uid: string) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: () => api.post<InvoiceView>(`/api/invoices/${uid}/pay-by-transfer`),
		onSuccess: () => invalidateInvoices(qc)
	})
}

/** Throws the draft away; its bookings return to the unbilled set. */
export function useDiscardDraft(uid: string) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: () => api.delete<void>(`/api/invoices/${uid}`),
		onSuccess: () => invalidateInvoices(qc)
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

/** The drafted invoice, plus the gateway URL when a `PaymentProvider` opened a payment. */
export type CheckoutResult = InvoiceView & { redirectUrl: string | null }

/** `null` means 204: there was nothing unbilled to check out. A `TRANSFER` checkout comes
 *  back already ISSUED with no redirect; a `CARD` one comes back DRAFT with one. */
export function useCheckout() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: CheckoutRequest) =>
			api.post<CheckoutResult | null>('/api/bookings/checkout', body),
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
