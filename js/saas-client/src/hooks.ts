// The framework's own routes as React Query hooks: a URL, a key, and what it invalidates.
// Nothing here decides anything — the rules live in the backend service handles.
//
// Every URL is an exported constant in `urls`. An application that mounts a framework route
// behind its own prefix — `examples/invoicing` does, for the invoice writes, because
// `invoice.org_read` owns `/api/invoices` and a duplicate axum path panics at startup —
// builds its own hook from `api.*` and does not reach for one of these.

import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import {
	acceptInvite,
	checkout,
	createRef,
	getEntitlements,
	listInvites,
	listOffers,
	listRefs,
	listSubscriptions,
	reactivateRef,
	revokeRef,
	type SubscriptionAction,
	subscriptionAction
} from './commerce'
import { api } from './http'
import type {
	ApiKeyView,
	BillingParty,
	CurrencyView,
	InvoiceView,
	LegalKind,
	MintedKey,
	NavCredentials,
	NavCredentialsStatus,
	Page,
	PasskeyView,
	PayMethod,
	RegisteredScopes,
	SellerView,
	ServiceView
} from './types'

export const urls = {
	services: '/api/services',
	service: (uid: string) => `/api/services/${uid}`,
	parties: '/api/billing-parties',
	party: (uid: string) => `/api/billing-parties/${uid}`,
	invoices: '/api/invoices',
	invoice: (uid: string) => `/api/invoices/${uid}`,
	invoicePdf: (uid: string) => `/api/invoices/${uid}/pdf`,
	currencies: '/api/currencies',
	seller: '/api/seller',
	sellerClosed: '/api/seller/closed',
	sellerPaymentDays: '/api/seller/payment-days',
	navCredentials: '/api/nav/credentials',
	consents: '/api/consents',
	consent: (kind: LegalKind) => `/api/consents/${kind}`,
	stepUp: '/api/auth/step-up',
	accountDelete: '/api/account/delete',
	apiKeys: '/api/api-keys',
	apiKeyScopes: '/api/api-keys/scopes',
	apiKey: (uid: string) => `/api/api-keys/${uid}`,
	passkeys: '/api/auth/wa/credentials',
	passkey: (credentialId: string) => `/api/auth/wa/credentials/${credentialId}`
}

/**
 * `GET /api/consents` answers an `Items<T>` — a bare `items` array, no cursor
 * (`crates/saas-auth/src/consent.rs`).
 */
export interface Consent {
	kind: LegalKind
	docVersion: string
	docSha256: string
	granted: boolean
	at: string
	withdrawnAt: string | null
	orgUid: string | null
}

/**
 * `invoice(uid)` sits *under* `invoices`, so invalidating the list also refetches every
 * open detail — one invalidation after any invoice mutation. An application that hangs its
 * own per-invoice keys off `['invoices', uid, …]` inherits that.
 */
export const keys = {
	services: ['services'] as const,
	parties: ['parties'] as const,
	invoices: ['invoices'] as const,
	invoice: (uid: string) => ['invoices', uid] as const,
	currencies: ['currencies'] as const,
	seller: ['seller'] as const,
	navCredentials: ['nav-credentials'] as const,
	consents: ['consents'] as const,
	// `apiKeyScopes` sits under `apiKeys`, so one invalidation after a mint refreshes the
	// listing the mint button renders beside.
	apiKeys: ['api-keys'] as const,
	apiKeyScopes: ['api-keys', 'scopes'] as const,
	passkeys: ['passkeys'] as const,
	offers: ['offers'] as const,
	subscriptions: ['subscriptions'] as const,
	entitlements: ['entitlements'] as const,
	refs: ['refs'] as const,
	invites: ['invites'] as const
}

/** `?active=0` is the one value that shows deactivated rows; anything else, absent included,
 *  hides them. The key varies with it, under `keys.services`, so one invalidation refetches both. */
export function useServices(includeInactive = false) {
	return useQuery({
		queryKey: includeInactive ? [...keys.services, 'all'] : keys.services,
		queryFn: ({ signal }) =>
			api.get<Page<ServiceView>>(
				includeInactive ? `${urls.services}?active=0` : urls.services,
				signal
			)
	})
}

export function useSaveService(uid: string | null) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: Record<string, unknown>) =>
			uid
				? api.patch<ServiceView>(urls.service(uid), body)
				: api.post<ServiceView>(urls.services, body),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.services })
	})
}

/**
 * Both directions through the patch. A service is never deleted — `invoice_lines.service_id`
 * references the row — and `DELETE /api/services/{uid}` is itself `{ active: false }`
 * (`crates/saas-invoice/src/catalog.rs`), which the method cannot reverse.
 */
export function useSetServiceActive() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (d: { uid: string; active: boolean }) =>
			api.patch<ServiceView>(urls.service(d.uid), { active: d.active }),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.services })
	})
}

export function useSeller() {
	return useQuery({
		queryKey: keys.seller,
		queryFn: ({ signal }) => api.get<SellerView>(urls.seller, signal)
	})
}

/** `POST /api/seller` — the acting org's Admin mints its own seller, once. */
export function useCreateSeller() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: Record<string, unknown>) => api.post<SellerView>(urls.seller, body),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.seller })
	})
}

/** `PUT /api/seller` — resolves to `null` on `204`: nothing changed, no version was made. */
export function useSyncSeller() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: Record<string, unknown>) =>
			api.put<SellerView | null>(urls.seller, body),
		// A new tax number drops the NAV connection server-side.
		onSuccess: () => {
			void qc.invalidateQueries({ queryKey: keys.seller })
			void qc.invalidateQueries({ queryKey: keys.navCredentials })
		}
	})
}

/** `PUT /api/seller/closed` — the Owner makes the company read-only or reopens it. Step-up. */
export function useSetSellerClosed() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (closed: boolean) => api.put<SellerView>(urls.sellerClosed, { closed }),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.seller })
	})
}

/** `PUT /api/seller/payment-days` — the Admin sets the org's payment term; null clears it. */
export function useSetSellerPaymentDays() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (paymentDays: number | null) =>
			api.put<SellerView>(urls.sellerPaymentDays, { paymentDays }),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.seller })
	})
}

/** A `403` for a Member: the status is an Admin's business. */
export function useNavCredentials(enabled = true) {
	return useQuery({
		queryKey: keys.navCredentials,
		queryFn: ({ signal }) => api.get<NavCredentialsStatus>(urls.navCredentials, signal),
		enabled,
		retry: false
	})
}

/** Step-up gated, and verified with one NAV `tokenExchange` before anything is stored. */
export function useSetNavCredentials() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: NavCredentials) =>
			api.put<NavCredentialsStatus>(urls.navCredentials, body),
		onSuccess: (status) => qc.setQueryData(keys.navCredentials, status)
	})
}

export function useParties() {
	return useQuery({
		queryKey: keys.parties,
		queryFn: ({ signal }) => api.get<Page<BillingParty>>(urls.parties, signal)
	})
}

/**
 * The one paged list: an account accumulates invoices forever, and page one presented as the
 * whole list is a customer who cannot see an invoice they were charged for. The other lists
 * stay single-page and say so when `nextCursor` is non-null.
 */
export function useInvoices(filter: { status?: string; q?: string; limit?: number } = {}) {
	const { status = '', q = '', limit = 0 } = filter
	return useInfiniteQuery({
		// Under `keys.invoices`, so one invalidation after a write refetches every filtered
		// view a screen is holding, not just the one that made it.
		queryKey: [...keys.invoices, 'list', status, q, limit] as const,
		queryFn: ({ pageParam, signal }) => {
			const p = new URLSearchParams()
			if (pageParam) p.set('cursor', pageParam)
			// Comma-separated status names, and a blank `q` is absent rather than an empty
			// match (`crates/saas-invoice/src/routes.rs::ListQuery`).
			if (status) p.set('status', status)
			if (q) p.set('q', q)
			if (limit) p.set('limit', String(limit))
			const qs = p.toString()
			return api.get<Page<InvoiceView>>(qs ? `${urls.invoices}?${qs}` : urls.invoices, signal)
		},
		initialPageParam: '',
		getNextPageParam: (last) => last.nextCursor ?? undefined
	})
}

/**
 * The single read: `lines` and `vatSummary` are omitted from the listing.
 *
 * `refetchInterval` is the caller's, because the one thing worth polling for — the PDF the
 * render job writes after issuing — is only pending for a moment, and a permanent poll on
 * every open invoice would be a page that never idles.
 */
export function useInvoice(uid: string, refetchInterval: number | false = false) {
	return useQuery({
		queryKey: keys.invoice(uid),
		queryFn: ({ signal }) => api.get<InvoiceView>(urls.invoice(uid), signal),
		enabled: uid !== '',
		refetchInterval
	})
}

/** Deployment-wide, not org-scoped: `enabled: false` rows are the ones an invoice may no
 *  longer be denominated in. */
export function useCurrencies() {
	return useQuery({
		queryKey: keys.currencies,
		queryFn: ({ signal }) => api.get<Page<CurrencyView>>(urls.currencies, signal)
	})
}

export function useSaveParty(uid: string | null) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: Record<string, unknown>) =>
			uid
				? api.patch<BillingParty>(urls.party(uid), body)
				: api.post<BillingParty>(urls.parties, body),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.parties })
	})
}

/** A hard delete, unlike a service's: an issued invoice froze its own copy of the buyer, so
 *  nothing references the row. */
export function useDeleteParty() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (uid: string) => api.delete<void>(urls.party(uid)),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.parties })
	})
}

export function useConsents() {
	return useQuery({
		queryKey: keys.consents,
		queryFn: ({ signal }) => api.get<{ items: Consent[] }>(urls.consents, signal)
	})
}

export function useRecordConsent() {
	const qc = useQueryClient()
	return useMutation({
		// No `orgUid`: TOS and PRIVACY are account-wide, and an org-scoped grant satisfies
		// the gate for nothing while `GET /api/consents` still reports it (`E-AUTH-CONSENT-SCOPE`).
		mutationFn: (d: { kind: LegalKind; version: string; docSha256: string }) =>
			api.post<void>(urls.consents, d),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.consents })
	})
}

export function useWithdrawConsent() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (kind: LegalKind) => api.delete<void>(urls.consent(kind)),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.consents })
	})
}

/** Re-auth for the step-up gate. A code with no password is refused server-side. */
export function useStepUp() {
	return useMutation({
		mutationFn: (password: string) => api.post<void>(urls.stepUp, { password })
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
			}>(urls.accountDelete, { confirmEmail })
	})
}

// --- API keys ---

export function useApiKeys() {
	return useQuery({
		queryKey: keys.apiKeys,
		queryFn: ({ signal }) => api.get<{ items: ApiKeyView[] }>(urls.apiKeys, signal)
	})
}

/** What a mint may ask for. Prefixes only — the verb follows the HTTP method at the route. */
export function useApiKeyScopes() {
	return useQuery({
		queryKey: keys.apiKeyScopes,
		queryFn: ({ signal }) => api.get<RegisteredScopes>(urls.apiKeyScopes, signal)
	})
}

/** Resolves to the plaintext key, which exists in exactly this one response. */
export function useCreateApiKey() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (body: { name: string; scopes: string[]; expiresAt?: string }) =>
			api.post<MintedKey>(urls.apiKeys, body),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.apiKeys })
	})
}

/** Rename only: scopes are frozen at mint, so widening a key means minting another. */
export function useRenameApiKey() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (d: { uid: string; name: string }) =>
			api.patch<void>(urls.apiKey(d.uid), { name: d.name }),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.apiKeys })
	})
}

/** No step-up: revoking a leaked key is the emergency action, and a re-auth prompt in
 *  front of it keeps the key live for the length of the prompt. */
export function useRevokeApiKey() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (uid: string) => api.delete<void>(urls.apiKey(uid)),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.apiKeys })
	})
}

// --- passkeys ---

export function usePasskeys() {
	return useQuery({
		queryKey: keys.passkeys,
		queryFn: ({ signal }) => api.get<{ items: PasskeyView[] }>(urls.passkeys, signal)
	})
}

export function useRenamePasskey() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (d: { credentialId: string; name: string }) =>
			api.patch<void>(urls.passkey(d.credentialId), { name: d.name }),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.passkeys })
	})
}

/** Step-up gated, unlike revoking a key: this takes a credential away, it does not shut one down. */
export function useRemovePasskey() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (credentialId: string) => api.delete<void>(urls.passkey(credentialId)),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.passkeys })
	})
}


// --- commerce (the functions in `commerce.ts`) ---

export function useOffers() {
	return useQuery({ queryKey: keys.offers, queryFn: ({ signal }) => listOffers(signal) })
}

export function useSubscriptions() {
	return useQuery({
		queryKey: keys.subscriptions,
		queryFn: ({ signal }) => listSubscriptions(signal)
	})
}

/** What the acting org may do now. Refetched after every checkout and subscription action,
 *  and on a `402` the caller should invalidate it too: the balance it shows is stale. */
export function useEntitlements() {
	return useQuery({ queryKey: keys.entitlements, queryFn: ({ signal }) => getEntitlements(signal) })
}

/** Both the subscription list and the entitlements move on a commit. */
function useCommerceInvalidate() {
	const qc = useQueryClient()
	return () => {
		void qc.invalidateQueries({ queryKey: keys.subscriptions })
		void qc.invalidateQueries({ queryKey: keys.entitlements })
	}
}

export function useCheckout() {
	const invalidate = useCommerceInvalidate()
	return useMutation({
		mutationFn: (d: { quoteToken: string; payMethod: PayMethod }) =>
			checkout(d.quoteToken, d.payMethod),
		onSuccess: invalidate
	})
}

export function useSubscriptionAction() {
	const invalidate = useCommerceInvalidate()
	return useMutation({
		mutationFn: (d: { uid: string; action: SubscriptionAction }) =>
			subscriptionAction(d.uid, d.action),
		onSuccess: invalidate
	})
}

export function useRefs(type?: string) {
	return useQuery({
		queryKey: [...keys.refs, type ?? ''],
		queryFn: ({ signal }) => listRefs(type, signal)
	})
}

export function useCreateRef() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: createRef,
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.refs })
	})
}

export function useRevokeRef() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: revokeRef,
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.refs })
	})
}

export function useReactivateRef() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: reactivateRef,
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.refs })
	})
}

export function useInvites() {
	return useQuery({ queryKey: keys.invites, queryFn: ({ signal }) => listInvites(signal) })
}

/** Joining an org changes what every org-scoped query answers; the caller reloads the session. */
export function useAcceptInvite() {
	const qc = useQueryClient()
	return useMutation({ mutationFn: acceptInvite, onSuccess: () => qc.invalidateQueries() })
}

// vim: ts=4
