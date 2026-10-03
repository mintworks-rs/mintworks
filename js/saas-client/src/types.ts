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
	/** Overrides the seller's payment term; null inherits it. */
	paymentDays: number | null
	/** The method a new draft for this party starts with; null is TRANSFER. */
	paymentMethod: 'TRANSFER' | 'CASH' | null
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
	 *  `to_decimal_string` (`saas-core/src/money.rs`). */
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
	/** The booking's date and free-text note. */
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
/**
 * `GET /api/seller` — the acting org's seller: the statutory data of the `CURRENT`
 * `seller_versions` row plus the live `sellers.series_code`.
 *
 * No currency field. An org's base currency is `LoginBody.org.billingCurrency`, falling back
 * to the deployment's `currency.base` setting — the seller does not carry one.
 */
export interface SellerView {
	uid: string
	name: string
	country: string
	taxNumber: string
	groupMemberTaxNo: string | null
	euVatId: string | null
	postcode: string
	city: string
	street: string
	bankAccount: string | null
	bankName: string | null
	smallBusiness: boolean
	/** `NORMAL` or `ALANYI_MENTES`; the income-tax regime is `incomeRegime`. */
	vatScheme: string
	/** `NONE`, `KATA` or `ATALANY`. */
	incomeRegime: string
	/** The átalányadó költséghányad; `null` is the year's general rate. */
	expenseRatioPct: number | null
	/** `YYYY-MM-DD` the current tax status started, when mid-year. */
	regimeSince: string | null
	seriesCode: string
	/** Which `seller_versions` row this is; an invoice names its own through `sellerVer`. */
	sellerVer: number
	status: 'DRAFT' | 'CURRENT' | 'ARCHIVED'
	validFrom: string | null
	supersededAt: string | null
	/** The acting org invoices under an ancestor's seller, not one of its own. */
	inherited: boolean
	/** Read-only since: only payments can be recorded. */
	closedAt: string | null
	/** An invoice has a number, so the tax number is fixed: a new one is a new company. */
	taxNumberLocked: boolean
	/** The org's own payment term; null inherits `defaultPaymentDays`. */
	paymentDays: number | null
	/** The deployment's `invoice.default_payment_days`. */
	defaultPaymentDays: number
}

/** What a caller may know about a secret: whether it is set and when it changed. Never the value. */
export interface SecretStatus {
	set: boolean
	updatedAt: string | null
}

/** `GET`/`PUT /api/nav/credentials` — the acting org's seller's NAV connection. */
export interface NavCredentialsStatus {
	login: string | null
	/** The login and all three secrets are set. */
	connected: boolean
	/** Issued invoices not yet reported to NAV — what connecting releases. */
	unreported: number
	techPassword: SecretStatus
	signKey: SecretStatus
	exchangeKey: SecretStatus
}

/** `PUT /api/nav/credentials` body. Write-only: nothing ever reads the three secrets back. */
export interface NavCredentials {
	login: string
	techPassword: string
	signKey: string
	exchangeKey: string
}

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

// --- the dashboard summary (`GET /api/app/summary`) ---
//
// Every bucket is keyed by currency and nothing is converted: no rate applies to a mixed sum,
// so a consumer presents one group per currency and never one summed number.

/** One `(status, currency)` pair. `paid` is what has been received against that bucket, so an
 *  `ISSUED` bucket's outstanding amount is `gross - paid`. */
export interface StatusBucket {
	status: InvoiceStatus
	currency: string
	count: number
	net: MoneyWire
	vat: MoneyWire
	gross: MoneyWire
	paid: MoneyWire
}

/** `'YYYY-MM'` off the fulfilment date, falling back to `issuedAt`. A draft has no number and
 *  never appears here, so the chart is issued business only. */
export interface MonthBucket {
	month: string
	currency: string
	count: number
	gross: MoneyWire
	paid: MoneyWire
}

/** `ISSUED`, past its due date, paid short of gross. */
export interface OverdueBucket {
	currency: string
	count: number
	outstanding: MoneyWire
}

export interface InvoiceSummary {
	statuses: StatusBucket[]
	months: MonthBucket[]
	overdue: OverdueBucket[]
	/** One amount per currency, by payment date in the current Budapest month — unlike
	 *  `months[].paid`, which is by fulfilment month. */
	paidThisMonth: MoneyWire[]
}

/** `GET /api/currencies` (`crates/saas-invoice/src/catalog.rs::CurrencyView`), answered as a
 *  `Page`. `rate` is the base → this rate as a decimal string, null when none applies. */
export interface CurrencyView {
	code: string
	priceRoundStep: number
	/** Minor units a cash payment rounds to (HUF: 500); null = no rounding. */
	cashRoundStep: number | null
	mode: 'FIXED' | 'OFFICIAL'
	rate: string | null
	feeBp: number
	enabled: boolean
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

// --- agent (saas-agent) ---

export type AgentRunStatus = 'queued' | 'running' | 'done' | 'error' | 'cancelled' | 'interrupted'

/** A model-requested call. `arguments` is the raw JSON string the model produced. */
export interface AgentToolCall {
	id: string
	name: string
	arguments: string
}

/** One event of `GET /api/agent/runs/{uid}/events`: the SSE `id` is `seq`, `event` is `kind`,
 *  `data` the payload. A run's last event is `done` or `error`. */
export type AgentRunEvent = { seq: number } & (
	| { kind: 'queued'; data: Record<string, never> }
	| { kind: 'delta'; data: { text: string } }
	| {
			kind: 'message'
			data: { role: 'assistant'; content: string; model: string; toolCalls?: AgentToolCall[] }
	  }
	| { kind: 'tool_call'; data: AgentToolCall }
	| {
			kind: 'tool_result'
			data: { id: string; name: string } & (
				| { ok: true; result: unknown }
				| { ok: false; error: string }
			)
	  }
	| { kind: 'done'; data: { status: 'done' | 'cancelled' } }
	| {
			kind: 'error'
			data: { status: 'error' | 'interrupted'; errCode?: string; errStr?: string }
	  }
)


// --- refs (saas-core/src/refs.rs), members (saas-auth/src/org.rs) ---

/** `signup` and `org_invite` are minted by saas-auth; any other type is the app's own. */
export interface Ref {
	uid: string
	code: string
	type: string
	target: string | null
	/** The only address that may use it; `null` = anyone. */
	email: string | null
	params: unknown
	/** `null` = unlimited. */
	usesLeft: number | null
	expiresAt: string | null
	status: 'ACTIVE' | 'REVOKED'
	createdAt: string
}

/** `POST /api/refs`; `type` is fixed (and may be omitted) on `POST /api/auth/signup-refs`. */
export interface CreateRef {
	type?: string
	/** A chosen slug; absent mints a random code. */
	code?: string
	target?: string
	email?: string
	params?: unknown
	usesLeft?: number
	expiresAt?: string
}

/** `GET /api/refs/{code}`, public. No reason is given when `valid` is false. */
export interface RefPreview {
	type: string
	valid: boolean
	orgName?: string
}

/** `accountUid`, `email` and `status` are absent only in a role-change answer for an unanswered invitation; the listing returns accepted members only. */
export interface Member {
	accountUid?: string
	email?: string
	name?: string
	role: Role
	status?: string
	accepted: boolean
	createdAt: string
}

/** `POST /api/auth/register`. `ref` is a signup/affiliate code or an org invitation. */
export interface RegisterBody {
	email: string
	name?: string
	locale?: string
	consents: { kind: LegalKind; version: string }[]
	pow: PowProof
	ref?: string
}

// --- entitlements (saas-entitle/src/{service,store}.rs) ---

export interface Entitlements {
	features: string[]
	limits: Record<string, number>
	meters: Record<string, { balance: number; nextExpiry: string | null }>
}

export type GrantSource = 'SUBSCRIPTION' | 'PURCHASE' | 'REWARD' | 'MANUAL' | 'TRIAL'

export interface Grant {
	uid: string
	key: string
	amount: number
	validFrom: string
	/** `null` = forever. */
	validUntil: string | null
	source: GrantSource
	sourceRef: string | null
	createdAt: string
	used: number
}

// --- plans (saas-plans/src/{service,quote,checkout,store,admin}.rs) ---

export interface OfferView {
	uid: string
	code: string
	name: string
	kind: 'ONE_TIME' | 'RECURRING'
	family: string | null
	/** Higher is the bigger tier within a family. */
	rank: number
	interval: 'MONTH' | 'YEAR' | null
	intervalCount: number | null
	validityDays: number | null
	trialDays: number
	prices: MoneyWire[]
	entitlements: { key: string; amount: number; perSeat: boolean }[]
}

/** With `subscription` set it quotes a tier or seat change of that subscription. */
export interface QuoteReq {
	offer: string
	qty?: number
	currency?: string
	coupon?: string
	subscription?: string
}

export interface QuoteLine {
	description: string
	unit: string
	qty: number
	unitPrice: MoneyWire
	net: MoneyWire
	vat: MoneyWire
	gross: MoneyWire
}

/**
 * `effective: 'now'` — a purchase, or an upgrade billed pro rata for the rest of the period.
 * `'period_end'` — a downgrade: totals are zero and the change waits for the renewal.
 */
export interface Quote {
	lines: QuoteLine[]
	net: MoneyWire
	vat: MoneyWire
	gross: MoneyWire
	currency: string
	periodStart?: string
	periodEnd?: string
	effective: 'now' | 'period_end'
	quoteToken: string
}

/** `pay`: card payment awaits (`POST /api/invoices/{uid}/pay`); `issued`: transfer invoice;
 *  `trialing`: nothing billed yet; `scheduled`: a downgrade queued, no invoice. */
export interface Checkout {
	invoiceUid?: string
	subscriptionUid?: string
	next: 'pay' | 'issued' | 'trialing' | 'scheduled'
}

export type SubStatus = 'TRIALING' | 'ACTIVE' | 'PAST_DUE' | 'SUSPENDED' | 'CANCELED'

/** No offer code on the wire: match by `family` against `OfferView.family`. */
export interface Subscription {
	uid: string
	family: string | null
	qty: number
	status: SubStatus
	currency: string
	/** Per seat per period, a decimal string in `currency`; grandfathered. */
	price: string
	periodStart: string
	periodEnd: string
	cancelAtPeriodEnd: boolean
	/** A queued seat change; a queued tier is not exposed. */
	nextQty: number | null
	payMethod: PayMethod
	couponPeriodsLeft: number | null
	createdAt: string
	updatedAt: string
}

/** The operator's cross-org list carries the org each subscription belongs to. */
export interface AdminSubscription extends Subscription {
	orgUid: string
}

export interface AdminCancelReq {
	immediate: boolean
	/** `prorated` needs `immediate`. */
	refund?: 'none' | 'prorated'
}

// vim: ts=4
