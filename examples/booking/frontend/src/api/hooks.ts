// The example's own server calls — bookings, checkout, payments and the NAV panel, which
// `@saas-framework/client` deliberately does not ship. Everything the framework serves is there.

import { useEffect, useRef } from 'react'

import type { QueryClient } from '@tanstack/react-query'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import type {
	InvoiceView,
	NavSubmission,
	Page,
	PaymentState,
	PaymentView,
	ProviderView
} from '@saas-framework/client'
import { api, keys as frameworkKeys } from '@saas-framework/client'

import type { BookRequest, Booking, CheckoutRequest } from '~/api/types'

/**
 * The framework's keys plus this example's. `nav(uid)` and `payments(uid)` hang off
 * `['invoices', uid, …]` on purpose, so the package's one invalidation after any invoice
 * mutation refetches them too.
 */
export const keys = {
	...frameworkKeys,
	bookings: ['bookings'] as const,
	nav: (uid: string) => ['invoices', uid, 'nav'] as const,
	payments: (uid: string) => ['invoices', uid, 'payments'] as const,
	providers: ['payment-providers'] as const
}

/** The four live states. Anything else is terminal and the page stops polling. */
const LIVE: PaymentState[] = ['PENDING', 'AWAITING_USER', 'RESERVED', 'AUTHORIZED']

// `payments(uid)` and `invoice(uid)` both sit under `invoices`, so this refetches them too.
// `bookings` is here because a booking's `invoiceUid` moves with the draft.
function invalidateInvoices(qc: QueryClient) {
	qc.invalidateQueries({ queryKey: keys.invoices })
	qc.invalidateQueries({ queryKey: keys.bookings })
}

export function useBookings() {
	return useQuery({
		queryKey: keys.bookings,
		queryFn: ({ signal }) => api.get<Page<Booking>>('/api/bookings', signal)
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
 * An empty list means card is not offered — the offline demo, with no `PAYMENT_BARION_POS_KEY`
 * set, which is what `main.rs` gates registering the gateway on.
 */
export function useProviders() {
	return useQuery({
		queryKey: keys.providers,
		queryFn: ({ signal }) => api.get<Page<ProviderView>>('/api/payment-providers', signal)
	})
}

/**
 * Every payment opened against one invoice, polled while one is live, because the gateway
 * settles out of band through its webhook and the return URL carries no state.
 *
 * The poll is a read of our own tables and costs the gateway nothing; the gateway is asked once
 * per live payment, by the return leg below. `PAYMENT_SWEEP` covers a payer who never lands
 * here at all.
 *
 * An interval, not a push. `app.tsx` turns `refetchOnWindowFocus` off globally, so
 * the interval is the only thing that would notice; SSE or a websocket is the upgrade if a demo
 * ever needs sub-second feedback.
 */
export function useInvoicePayments(uid: string) {
	const qc = useQueryClient()
	const query = useQuery({
		queryKey: keys.payments(uid),
		queryFn: ({ signal }) =>
			api.get<Page<PaymentView>>(`/api/invoices/${uid}/payments`, signal),
		enabled: uid !== '',
		refetchInterval: (q) => {
			const p = q.state.data?.items.find((p) => LIVE.includes(p.status))
			if (!p) return false
			// The deadline is the precise bound; the count is the floor under a row that carries none
			// — a pre-v11 row, or a start the gateway never stamped — which otherwise polled forever.
			if (p.expiresAt && Date.now() > Date.parse(p.expiresAt) + 30_000) return false
			return q.state.dataUpdateCount < 150 ? 2000 : false
		}
	})

	// The return leg. The gateway's redirect carries no state and cannot reach a `localhost`
	// callback, so the payer landing on this page is what asks — once per payment, because every
	// ask is a round trip against a quota the read poll above deliberately does not spend.
	const asked = useRef(new Set<string>())
	useEffect(() => {
		for (const p of query.data?.items ?? []) {
			if (!LIVE.includes(p.status) || asked.current.has(p.uid)) continue
			asked.current.add(p.uid)
			// A refusal — a throttled or unreachable gateway — leaves the row as it was read: the
			// sweep asks again, and an error toast for a poll nobody requested is noise.
			void api
				.get<PaymentView>(`/api/payments/${p.uid}`)
				.then(() => qc.invalidateQueries({ queryKey: keys.payments(uid) }))
				.catch(() => {})
		}
	}, [query.data, qc, uid])

	// A changed payment status is not an invalidation: without this the detail keeps serving the
	// cached DRAFT until a manual reload.
	const seen = useRef<string | undefined>(undefined)
	const statuses = query.data?.items.map((p) => p.status).join(',')
	useEffect(() => {
		if (statuses === undefined) return
		const previous = seen.current
		seen.current = statuses
		// `keys.invoice(uid)` is a *prefix* of `keys.payments(uid)`, so a non-exact invalidation
		// would refetch this very query and read it twice per change. The first sample counts: on
		// the return from the gateway it is the response to the ask above that issued the invoice,
		// and the `useInvoice` GET racing beside it answered PENDING.
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

// vim: ts=4
