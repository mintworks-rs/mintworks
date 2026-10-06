// Refs, invitations, entitlements and plans: one typed function per route. The React Query
// hooks over the reads live in `hooks.ts`; these are for everything else, and for the hooks.
//
// A tier or seat change is the purchase flow with `subscription` set: `quote` shows the price
// (or the zero-amount downgrade and its `periodEnd`), `checkout` commits exactly that quote.

import { api } from './http'
import type {
	AdminCancelReq,
	AdminSubscription,
	Checkout,
	CreateRef,
	Entitlements,
	Grant,
	Member,
	MoneyWire,
	OfferView,
	PayMethod,
	Quote,
	QuoteReq,
	Ref,
	RefPreview,
	RegisterBody,
	Subscription,
	SubStatus
} from './types'

const enc = encodeURIComponent

// --- registration and invitations (mintworks-auth) ---

/** No token and no uid come back: the account has to be activated first. */
export const register = (body: RegisterBody) => api.post<void>('/api/auth/register', body)

/** The org's invitations, pending and used. */
export const listInvites = (signal?: AbortSignal) =>
	api.get<{ items: Ref[] }>('/api/org/invites', signal)

/** Mints an `org_invite` for `email`; no account is created until it is accepted. */
export const inviteMember = (email: string, role: Member['role']) =>
	api.post<void>('/api/org/members', { email, role })

export const listMembers = (signal?: AbortSignal) =>
	api.get<{ items: Member[] }>('/api/org/members', signal)

/** The signed-in account joins the inviting org. Call `useAuth().reload()` after, for `orgs`. */
export const acceptInvite = (code: string) =>
	api.post<void>(`/api/org/invites/${enc(code)}/accept`)

export const createSignupRef = (body: CreateRef) => api.post<Ref>('/api/auth/signup-refs', body)

// --- refs (mintworks-core) ---

/** Public: what a `/r/:code` landing page shows before sign-up. */
export const previewRef = (code: string, signal?: AbortSignal) =>
	api.get<RefPreview>(`/api/refs/${enc(code)}`, signal)

export const listRefs = (type?: string, signal?: AbortSignal) =>
	api.get<{ items: Ref[] }>(type ? `/api/refs?type=${enc(type)}` : '/api/refs', signal)

export const createRef = (body: CreateRef) => api.post<Ref>('/api/refs', body)

export const revokeRef = (uid: string) => api.delete<void>(`/api/refs/${enc(uid)}`)

/** Back to active; expiry and uses left still apply. */
export const reactivateRef = (uid: string) => api.post<void>(`/api/refs/${enc(uid)}/reactivate`)

// --- entitlements (mintworks-entitle) ---

export const getEntitlements = (signal?: AbortSignal) =>
	api.get<Entitlements>('/api/entitlements', signal)

/** Operator only. */
export const listGrants = (orgUid: string, signal?: AbortSignal) =>
	api.get<{ items: Grant[] }>(`/api/admin/orgs/${enc(orgUid)}/grants`, signal)

/** Operator only: a `MANUAL` grant. */
export const addGrant = (orgUid: string, body: { key: string; amount: number; validUntil?: string }) =>
	api.post<Grant>(`/api/admin/orgs/${enc(orgUid)}/grants`, body)

// --- plans (mintworks-plans) ---

/** Public. */
export const listOffers = (signal?: AbortSignal) =>
	api.get<{ items: OfferView[] }>('/api/plans/offers', signal)

export const quote = (body: QuoteReq) => api.post<Quote>('/api/plans/quote', body)

/** `E-PLAN-QUOTE-STALE` (409) or `-EXPIRED` (410): quote again and show the new price. */
export const checkout = (quoteToken: string, payMethod: PayMethod) =>
	api.post<Checkout>('/api/plans/checkout', { quoteToken, payMethod })

export const listSubscriptions = (signal?: AbortSignal) =>
	api.get<{ items: Subscription[] }>('/api/plans/subscriptions', signal)

/** `cancel` ends at `periodEnd`, `resume` undoes that, `cancel-change` drops a queued downgrade. */
export type SubscriptionAction = 'cancel' | 'resume' | 'cancel-change'

export const subscriptionAction = (uid: string, action: SubscriptionAction) =>
	api.post<Subscription>(`/api/plans/subscriptions/${enc(uid)}/${action}`)

// --- operator (crates/plans/src/admin.rs) ---

export const adminSubscriptions = (
	filter: { status?: SubStatus; org?: string } = {},
	signal?: AbortSignal
) => {
	const p = new URLSearchParams()
	if (filter.status) p.set('status', filter.status)
	if (filter.org) p.set('org', filter.org)
	const qs = p.toString()
	return api.get<{ items: AdminSubscription[] }>(
		qs ? `/api/admin/subscriptions?${qs}` : '/api/admin/subscriptions',
		signal
	)
}

/** Step-up gated. `immediate` cuts the period's grants now; `prorated` refunds the rest. */
export const adminCancel = (uid: string, body: AdminCancelReq) =>
	api.post<Subscription>(`/api/admin/subscriptions/${enc(uid)}/cancel`, body)

/** Rewrites live subscribers' price from their next renewal; answers the subscriptions it changed. */
export const reprice = (offerCode: string, price: MoneyWire) =>
	api.post<{ items: Subscription[] }>(`/api/admin/offers/${enc(offerCode)}/reprice`, price)

// vim: ts=4
