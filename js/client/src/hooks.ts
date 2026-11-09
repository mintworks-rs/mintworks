// SPDX-License-Identifier: MPL-2.0
// The framework's own routes as React Query hooks: a URL, a key, and what it invalidates.
// Nothing here decides anything — the rules live in the backend service handles.
//
// Every URL is an exported constant in `urls`. An application that mounts a framework route
// behind its own prefix — `examples/invoicing` does, for the invoice writes, because
// `invoice.org_read` owns `/api/invoices` and a duplicate axum path panics at startup —
// builds its own hook from `api.*` and does not reach for one of these.

import {
	keepPreviousData,
	useInfiniteQuery,
	useMutation,
	useQuery,
	useQueryClient
} from '@tanstack/react-query'

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
import { useAuth } from './auth/context'
import { api } from './http'
import type {
	Account,
	AccountPatch,
	ApiKeyView,
	BillingParty,
	CurrencyView,
	InvoiceMail,
	InvoiceView,
	MailIntro,
	MailSent,
	IssuePlan,
	LegalKind,
	MintedKey,
	NavCredentials,
	NavCredentialsStatus,
	NavCredentialsTest,
	Page,
	PasskeyView,
	PayMethod,
	RegisteredScopes,
	SellerSeries,
	SellerView,
	SeriesPatch,
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
	invoicePlan: (uid: string) => `/api/invoices/${uid}/plan`,
	invoicePreview: '/api/invoices/preview',
	invoiceSend: (uid: string) => `/api/invoices/${uid}/send`,
	invoiceMailOptout: (uid: string) => `/api/invoices/${uid}/mail-optout`,
	invoiceMails: (uid: string) => `/api/invoices/${uid}/mails`,
	mailIntro: '/api/invoice-mail/intro',
	currencies: '/api/currencies',
	seller: '/api/seller',
	sellerClosed: '/api/seller/closed',
	sellerPaymentDays: '/api/seller/payment-days',
	sellerSeries: '/api/seller/series',
	sellerSeriesOne: (code: string) => `/api/seller/series/${encodeURIComponent(code)}`,
	navCredentials: '/api/nav/credentials',
	navCredentialsTest: '/api/nav/credentials/test',
	navLive: '/api/nav/live',
	consents: '/api/consents',
	consent: (kind: LegalKind) => `/api/consents/${kind}`,
	stepUp: '/api/auth/step-up',
	account: '/api/account',
	accountDelete: '/api/account/delete',
	apiKeys: '/api/api-keys',
	apiKeyScopes: '/api/api-keys/scopes',
	apiKey: (uid: string) => `/api/api-keys/${uid}`,
	passkeys: '/api/auth/wa/credentials',
	passkey: (credentialId: string) => `/api/auth/wa/credentials/${credentialId}`
}

/**
 * `GET /api/consents` answers an `Items<T>` — a bare `items` array, no cursor
 * (`crates/auth/src/consent.rs`).
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
	invoiceMails: (uid: string) => ['invoices', uid, 'mails'] as const,
	mailIntro: ['invoice-mail-intro'] as const,
	currencies: ['currencies'] as const,
	seller: ['seller'] as const,
	sellerSeries: ['seller', 'series'] as const,
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
 *  hides them. `q` matches name or code on the server. The key varies with both, under
 *  `keys.services`, so one invalidation refetches every variant. */
export function useServices(includeInactive = false, q = '') {
	return useQuery({
		queryKey: [...keys.services, includeInactive ? 'all' : 'active', q],
		queryFn: ({ signal }) => {
			const p = new URLSearchParams()
			if (includeInactive) p.set('active', '0')
			if (q) p.set('q', q)
			const qs = p.toString()
			return api.get<Page<ServiceView>>(qs ? `${urls.services}?${qs}` : urls.services, signal)
		}
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
 * (`crates/invoice/src/catalog.rs`), which the method cannot reverse.
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

export function useSellerSeries() {
	return useQuery({
		queryKey: keys.sellerSeries,
		queryFn: ({ signal }) => api.get<Page<SellerSeries>>(urls.sellerSeries, signal)
	})
}

/** `isNew` POSTs the series, otherwise PATCHes it. Promoting a default rewrites
 *  the seller's `seriesCode`, so the seller is refetched too (`sellerSeries` sits under it). */
export function useSaveSeries() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (d: { code: string; isNew: boolean } & SeriesPatch) => {
			const { code, isNew, ...patch } = d
			return isNew
				? api.post<SellerSeries>(urls.sellerSeries, { code, ...patch })
				: api.patch<SellerSeries>(urls.sellerSeriesOne(code), patch)
		},
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.seller })
	})
}

/** Refused for the default series and for one an invoice uses. */
export function useDeleteSeries() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (code: string) => api.delete<void>(urls.sellerSeriesOne(code)),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.sellerSeries })
	})
}

/** What issuing the draft now would produce; the number is expected, not reserved. */
export function useIssuePlan(uid: string, enabled = true) {
	return useQuery({
		queryKey: [...keys.invoice(uid), 'plan'] as const,
		queryFn: ({ signal }) => api.get<IssuePlan>(urls.invoicePlan(uid), signal),
		enabled: enabled && uid !== ''
	})
}

/** `POST /api/invoices/preview` — a stored draft by uid, or a whole unsaved draft body;
 *  the PDF is watermarked and stored nowhere. */
export const previewInvoice = (
	body: { uid: string } | Record<string, unknown>,
	signal?: AbortSignal
) => api.blob(urls.invoicePreview, signal, body)

/** `PUT /api/nav/live` — ends test mode. Owner, step-up, credentials connected. */
export function useGoLive() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: () => api.put<void>(urls.navLive),
		onSuccess: () => {
			void qc.invalidateQueries({ queryKey: keys.seller })
			void qc.invalidateQueries({ queryKey: keys.invoices })
		}
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

/** Step-up gated; one NAV `tokenExchange` with the stored credentials, nothing is written. */
export function useTestNavCredentials() {
	return useMutation({
		mutationFn: () => api.post<NavCredentialsTest>(urls.navCredentialsTest)
	})
}

/** The caller's own handle, name and locale. Re-reads the session so `me.account` follows. */
export function useUpdateAccount() {
	const { reload } = useAuth()
	return useMutation({
		mutationFn: (patch: AccountPatch) => api.patch<Account>(urls.account, patch),
		onSuccess: () => reload()
	})
}

/** The `GET /api/billing-parties` filter (`crates/invoice/src/party.rs::PartyQuery`). `sort` is
 *  `name|taxNumber|createdAt`, `-` prefixed for descending; `limit` defaults to 200, clamped
 *  1..=200. */
export interface PartyFilter {
	q?: string
	sort?: string
	offset?: number
	limit?: number
}

/** The org's billing parties, each with its `openReceivable`, and their `total`. */
export function useParties(filter: PartyFilter = {}) {
	const { q = '', sort = '', offset = 0, limit = 0 } = filter
	return useQuery({
		queryKey: [...keys.parties, q, sort, offset, limit] as const,
		queryFn: async ({ signal }) => {
			const page = await api.get<Page<BillingParty>>(
				listUrl(urls.parties, { q, sort, offset, limit }),
				signal
			)
			return { items: page.items, total: page.total ?? 0 }
		}
	})
}

/** The `GET /api/invoices` filter (`crates/invoice/src/routes.rs::ListQuery`). `status` is a
 *  comma list that may mix invoice statuses with `NAV_ERROR`/`NAV_PENDING`/`NAV_OK`/`NAV_ACK`; `from`/`to`
 *  are inclusive `YYYY-MM-DD` issue dates; `party` a `prt_` uid; `kind` `NORMAL|STORNO`; `series`
 *  a series code; `currency` a code; `test` `test|live`; `overdue` `'true'` for unpaid live
 *  invoices past due; `email` `'problem'` for a missing or failed invoice email. `sort` is `issueDate|gross|dueDate|partner`, `-` prefixed for descending,
 *  and only `useInvoicePage` sends it: the server rejects `sort` together with a cursor. */
export interface InvoiceFilter {
	status?: string
	q?: string
	sort?: string
	from?: string
	to?: string
	party?: string
	kind?: string
	series?: string
	currency?: string
	test?: string
	overdue?: string
	email?: string
	limit?: number
}

/** The narrowing fields of an `InvoiceFilter`, in a fixed order for the query key. */
const NARROW = [
	'status',
	'q',
	'from',
	'to',
	'party',
	'kind',
	'series',
	'currency',
	'test',
	'overdue',
	'email'
] as const

function narrowOf(f: InvoiceFilter): Record<(typeof NARROW)[number], string> {
	return Object.fromEntries(NARROW.map((k) => [k, f[k] ?? ''])) as Record<
		(typeof NARROW)[number],
		string
	>
}

function listUrl(base: string, params: Record<string, string | number | undefined>): string {
	const p = new URLSearchParams()
	// A blank value is absent rather than an empty match; offset 0 is the first page.
	for (const [k, v] of Object.entries(params)) {
		if (v !== undefined && v !== '' && v !== 0) p.set(k, String(v))
	}
	const qs = p.toString()
	return qs ? `${base}?${qs}` : base
}

/**
 * The one paged list: an account accumulates invoices forever, and page one presented as the
 * whole list is a customer who cannot see an invoice they were charged for. The other lists
 * stay single-page and say so when `nextCursor` is non-null.
 */
export function useInvoices(filter: InvoiceFilter = {}) {
	const narrow = narrowOf(filter)
	const limit = filter.limit ?? 0
	return useInfiniteQuery({
		// Under `keys.invoices`, so one invalidation after a write refetches every filtered
		// view a screen is holding, not just the one that made it.
		queryKey: [...keys.invoices, 'list', ...NARROW.map((k) => narrow[k]), limit] as const,
		queryFn: ({ pageParam, signal }) =>
			api.get<Page<InvoiceView>>(
				listUrl(urls.invoices, { ...narrow, limit, cursor: pageParam }),
				signal
			),
		initialPageParam: '',
		getNextPageParam: (last) => last.nextCursor ?? undefined
	})
}

/** One offset-addressed page of `GET /api/invoices` and the filter's `total`, for a sortable
 *  table. Never sends a cursor (the server rejects one beside `sort`/`offset`). */
export function useInvoicePage(filter: InvoiceFilter & { offset?: number }) {
	const narrow = narrowOf(filter)
	const { sort = '', offset = 0, limit = 0 } = filter
	return useQuery({
		queryKey: [
			...keys.invoices,
			'page',
			...NARROW.map((k) => narrow[k]),
			sort,
			offset,
			limit
		] as const,
		queryFn: async ({ signal }) => {
			const page = await api.get<Page<InvoiceView>>(
				listUrl(urls.invoices, { ...narrow, sort, offset, limit }),
				signal
			)
			return { items: page.items, total: page.total ?? 0 }
		},
		// Keeps the previous page on screen while the next one loads.
		placeholderData: keepPreviousData
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

/** `GET /api/invoices/{uid}/mails`: the invoice's email attempts, newest first. Under
 *  `keys.invoice(uid)`, so a send's invalidation refetches it. */
export function useInvoiceMails(uid: string) {
	return useQuery({
		queryKey: keys.invoiceMails(uid),
		queryFn: ({ signal }) => api.get<InvoiceMail[]>(urls.invoiceMails(uid), signal),
		enabled: uid !== ''
	})
}

/** `POST /api/invoices/{uid}/send` — emails an issued invoice again (seller Admin). A party
 *  with no address answers `status: 'NO_EMAIL'` and writes nothing; the caps are 429
 *  `E-INV-RESEND-LIMIT` / `E-INV-MAIL-DAILY-LIMIT`. */
export function useSendInvoice(uid: string) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: () => api.post<MailSent>(urls.invoiceSend(uid), {}),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.invoice(uid) })
	})
}

/** `PUT /api/invoices/{uid}/mail-optout` — with `on`, neither the invoice nor its storno is
 *  mailed automatically (seller Admin). */
export function useSetMailOptout(uid: string) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (on: boolean) => api.put<void>(urls.invoiceMailOptout(uid), { on }),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.invoice(uid) })
	})
}

/** The acting org's intro text on invoice and storno emails. */
export function useMailIntro() {
	return useQuery({
		queryKey: keys.mailIntro,
		queryFn: ({ signal }) => api.get<MailIntro>(urls.mailIntro, signal)
	})
}

/** `PUT /api/invoice-mail/intro` (org Admin): at most 1000 characters each, no links. */
export function useSetMailIntro() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (intro: MailIntro) => api.put<void>(urls.mailIntro, intro),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.mailIntro })
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
