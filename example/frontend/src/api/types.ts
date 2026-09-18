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

/** `P` personal, `O` org. This app creates personal tenants only. */
export type TenantKind = 'P' | 'O'
export type Role = 'MEMBER' | 'ADMIN' | 'OWNER'

export interface Tenant {
	uid: string
	name: string
	kind: TenantKind
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
	tenant: Tenant | null
	tenants: { uid: string; name: string; kind: TenantKind; role: Role }[]
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

// `billing_parties.kind` on the wire. One letter, like `TenantKind`: the Rust enum in
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

export type InvoiceStatus = 'DRAFT' | 'ISSUED' | 'PAID' | 'STORNO' | 'STORNOED'

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

// vim: ts=4
