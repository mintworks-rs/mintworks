// Wire shapes, camelCase, mirroring the framework's serde views. Amounts are
// strings and are never parsed into a JavaScript number: Money is i64 minor
// units server-side and a float here would lose the cent.

export interface MoneyWire {
	amount: string
	currency: string
}

export interface Page<T> {
	items: T[]
	nextCursor: string | null
}

export interface ErrorBody {
	/** `fields` is present only on `E-CORE-VALIDATION`, keyed by field name and valued by an
	 *  **error code** (`E-CORE-FORMAT`, `E-CORE-RANGE`) — not by displayable prose, so a
	 *  consumer must map the code to text before showing it. */
	error: { errCode: string; errStr: string; fields?: Record<string, string> }
}

// --- proof of work (saas-auth/src/pow.rs) ---

/** The four scopes `GET /api/pow/challenge?scope=` accepts. */
export type PowScope = 'register' | 'resend-activation' | 'password-reset' | 'login'

export interface PowChallenge {
	salt: string
	/** Required leading zero *bits* of sha256(salt + String(nonce)). */
	difficulty: number
	exp: string
	sig: string
}

export interface PowProof {
	salt: string
	exp: string
	sig: string
	nonce: number
}

// --- auth (saas-auth/src/token.rs, consent.rs) ---

export type LegalKind = 'TOS' | 'PRIVACY' | 'EINVOICE' | 'WITHDRAWAL_WAIVER'

export interface Account {
	uid: string
	email: string
	name: string | null
	locale: string
	isOperator: boolean
}

/** Exactly one `ROOT` org exists, the platform. This app creates personal orgs only. */
export type OrgKind = 'ROOT' | 'PERSONAL' | 'SHARED'
export type Role = 'MEMBER' | 'ADMIN' | 'OWNER'

export interface Org {
	uid: string
	name: string
	kind: OrgKind
	role: Role
	billingCurrency: string | null
}

/** The body of POST /api/auth/login, /refresh and GET /api/auth/me. The token
 *  fields are present but deliberately unused — auth rides the HttpOnly cookies. */
export interface LoginBody {
	accessToken?: string
	refreshToken?: string
	expiresIn?: number
	account: Account
	org: Org | null
	orgs: { uid: string; name: string; kind: OrgKind; role: Role }[]
	consentsRequired: LegalKind[]
}

export interface LegalDoc {
	kind: LegalKind
	locale: string
	version: string
	title: string
	/** Verbatim Markdown — the consent record is about this exact text. */
	body: string
	sha256: string
	effectiveFrom: string
}

// --- invoicing (saas-invoice/src/routes.rs) ---

// `billing_parties.kind` on the wire, one letter: the Rust enum in
// `saas-invoice/src/store.rs` renames its variants, so a spelt-out `PRIVATE` is a 422.
export type PartyKind = 'P' | 'C'

export interface BillingParty {
	uid: string
	kind: PartyKind
	name: string
	country: string
	taxNumber: string | null
	euVatId: string | null
	groupTaxNo: string | null
	postcode: string | null
	city: string | null
	street: string | null
	email: string | null
	isDefault: boolean
	createdAt: string
	updatedAt: string
}

export interface ServiceView {
	uid: string
	code: string | null
	name: string
	description: string | null
	unit: string
	unitPrice: MoneyWire
	vatCode: string
	active: boolean
}

export interface LineView {
	lineNo: number
	description: string
	unit: string
	/** A decimal string with six places, like `MoneyWire.amount`: 2.5 hours is `'2.500000'`.
	 *  Not the raw 1e6 integer `Booking.qtyE6` carries — `Qty` serializes through
	 *  `to_decimal_string` (`saas-core/src/money.rs:210`). */
	qty: string
	unitPrice: MoneyWire
	discountKind: string | null
	discountValue: number | null
	discountAmount: MoneyWire
	discountDescription: string | null
	net: MoneyWire
	vatCode: string
	vatRateBp: number
	vat: MoneyWire
	gross: MoneyWire
	/** The booking's date and free-text note (arch-11-line-note). */
	note: string | null
}

export interface VatGroupView {
	vatCode: string
	vatRateBp: number
	net: MoneyWire
	vat: MoneyWire
	gross: MoneyWire
	netHuf: MoneyWire | null
	vatHuf: MoneyWire | null
	grossHuf: MoneyWire | null
}

export interface BuyerView {
	kind: PartyKind | null
	name: string | null
	country: string | null
	taxNumber: string | null
	euVatId: string | null
	groupTaxNo: string | null
	postcode: string | null
	city: string | null
	street: string | null
}

/** `PENDING` is a draft frozen while a gateway holds a charge against its total: unnumbered,
 *  never filed at NAV, and back to `DRAFT` if the payment fails. */
export type InvoiceStatus = 'DRAFT' | 'PENDING' | 'ISSUED' | 'PAID' | 'STORNO' | 'STORNOED'

export interface InvoiceView {
	uid: string
	number: string | null
	kind: string
	status: InvoiceStatus
	billingPartyUid: string | null
	originalInvoiceUid: string | null
	stornoInvoiceUid: string | null
	seriesCode: string | null
	issuedAt: string | null
	fulfilmentDate: string | null
	dueDate: string | null
	paymentMethod: string
	currency: string
	hufRate: string | null
	rateDate: string | null
	rateSource: string | null
	net: MoneyWire
	vat: MoneyWire
	gross: MoneyWire
	paidAmount: MoneyWire
	paidAt: string | null
	/** Every applicable Áfa tv. 169. § note key, in issue order. */
	vatNotes: string[]
	notes: string | null
	buyer: BuyerView | null
	/** Omitted on a listing, present on a single-invoice read. */
	lines?: LineView[]
	vatSummary?: VatGroupView[]
	document?: { kind: string; sha256: string; bytes: number; templateVersion: string }
	createdAt: string
	updatedAt: string
}

// --- NAV (saas-nav/src/submission.rs) ---
//
// No route serves this yet: saas-nav registers no HTTP routes and InvoiceView carries no
// NAV field, so the UI can only show the "not configured" state.

/** `nav_submissions.verdict`; null until NAV answers. Every variant is final. */
export type NavVerdict = 'DONE' | 'WARN' | 'REJECTED' | 'FAILED'

export interface NavSubmission {
	op: string
	verdict: NavVerdict | null
	submittedAt: string | null
	message: string | null
}

// --- payments (saas-billing) ---

/** `crates/saas-billing/src/provider.rs`. The first four are live; the rest are terminal. */
export type PaymentState =
	| 'PENDING'
	| 'AWAITING_USER'
	| 'RESERVED'
	| 'AUTHORIZED'
	| 'SUCCEEDED'
	| 'PARTIALLY_SUCCEEDED'
	| 'FAILED'
	| 'CANCELED'
	| 'EXPIRED'
	| 'REFUNDED'

export interface PaymentAllocationView {
	invoiceUid: string
	/** `null` on a draft, which has no number yet. */
	invoiceNumber: string | null
	amount: MoneyWire
	allocatedAt: string
}

export interface PaymentView {
	uid: string
	kind: string
	provider: string | null
	providerRef: string | null
	requestId: string | null
	status: PaymentState
	amount: MoneyWire
	refundedAmount: MoneyWire
	/** Where the gateway wants the browser sent; what "Continue payment" navigates to. */
	redirectUrl: string | null
	/** When the gateway must have given up; what the payment countdown reads. */
	expiresAt: string | null
	receivedAt: string | null
	extRef: string | null
	note: string | null
	createdAt: string
	updatedAt: string
	allocations: PaymentAllocationView[]
}

export interface ProviderView {
	id: string
	caps: { reservation: boolean; recurring: boolean; partialRefund: boolean }
}

/** What the customer chose at checkout. Cash is not offered by a web checkout. */
export type PayMethod = 'CARD' | 'TRANSFER'

export interface CheckoutRequest {
	method: PayMethod
	provider?: string
}

// --- bookings (the example's own extension) ---
export interface Booking {
	uid: string
	serviceCode: string
	/** YYYY-MM-DD. */
	occurredOn: string
	qtyE6: number
	note: string | null
	invoiceUid: string | null
}

export interface BookRequest {
	serviceCode: string
	occurredOn: string
	qtyE6: number
	note?: string | null
}

// --- API keys (saas-auth/src/apikey.rs) ---

/** `GET /api/api-keys`. `key` is never here: the plaintext exists only in the mint response. */
export interface ApiKeyView {
	uid: string
	name: string
	prefix: string
	scopes: string[]
	createdAt: string
	lastUsedAt: string | null
	expiresAt: string | null
}

/** `POST /api/api-keys` — the one response carrying the plaintext, shown once and never again. */
export interface MintedKey {
	uid: string
	name: string
	prefix: string
	key: string
	scopes: string[]
	createdAt: string
	expiresAt: string | null
}

/** `GET /api/api-keys/scopes` — prefixes only. The verb follows the HTTP method at the route. */
export interface RegisteredScopes {
	prefixes: string[]
}

// --- passkeys (saas-auth/src/webauthn.rs) ---

/** One row of `GET /api/auth/wa/credentials`. */
export interface PasskeyView {
	credentialId: string
	name: string
	createdAt: string
	lastUsedAt: string | null
}

/** Both challenge routes' response: the browser's own options, plus the blob that carries their
 *  server-side state. `options` is the WebAuthn JSON encoding (base64url strings), which
 *  `navigator.credentials` does not accept — `~/auth/webauthn.ts` does the conversion. */
export interface WaChallenge {
	options: unknown
	blob: string
}

// --- QR login (saas-auth/src/qr.rs) ---

/** `POST /api/auth/qr/init`. `secret` is the desktop's alone: it goes back in `x-qr-secret`. */
export interface QrInit {
	sessionId: string
	secret: string
	matchCode: string
}

/** `GET /api/auth/qr/{sessionId}/details` — evidence for the human holding the phone. The match
 *  code is *not* here: the phone types the one shown on the initiating screen, which is what
 *  makes the code a check rather than a value the server hands to whoever saw the QR. */
export interface QrDetails {
	browser: string
	ip: string | null
}

/** The three outcomes that are not a sign-in. Approval answers the login body instead. */
export type QrPending = 'pending' | 'denied' | 'expired'

// vim: ts=4
