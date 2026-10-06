//! Process-local token buckets, keyed by `(scope, ip | account | email)`.
//!
//! No table: a single-process deployment does not need shared state, and a bucket in the DB would
//! be a write per request. Exceeding a bucket is `Error::RateLimit(retry_after_secs)` → 429
//! `E-CORE-RATELIMIT` with a `Retry-After` header.
//!
//! A route asks for its own scope: `app.limits.check(&app.settings, "login.ip", ip).await?`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Instant;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;

use crate::app::App;
use crate::auth_mw::{Claims, ClientIp};
use crate::error::{ClResult, Error};
use crate::settings::Settings;

/// The blanket `ratelimit.default` budget every route without a scope of its own gets. Routes
/// that also charge a named scope charge both, which is the intended layering: the tight
/// bucket is a floor, not a replacement.
///
/// The outermost of the three tiers — see [`scoped_account_mw`] and [`charge_auth_failure`] for
/// the two inside `auth_mw::require_auth`. Layered *outside* authentication, so the budget is
/// charged before `auth_mw::verify` runs its account lookup.
///
/// A request with no resolved [`ClientIp`] is passed through: there is no key to bucket it on, and
/// failing it would break a consumer that mounts the router without
/// `into_make_service_with_connect_info`. `client_ip_mw` is layered unconditionally in
/// `AppBuilder::run`, so in a framework-wired deployment the only callers reaching here without one
/// are off-socket — which is the documented unlimited path. One-shot, from both middlewares: an
/// unlimited path is the documented behaviour, but a consumer mounting the router without
/// `into_make_service_with_connect_info` turned every limit off with nothing in the log. Once, so a
/// misconfigured deployment cannot flood it.
fn warn_missing_client_ip() {
	static MISSING_CLIENT_IP: std::sync::Once = std::sync::Once::new();
	MISSING_CLIENT_IP.call_once(|| {
		tracing::warn!(
			"no ClientIp extension: rate limiting is OFF for these requests; serve the \
			 router with into_make_service_with_connect_info"
		);
	});
}

pub async fn default_mw(State(app): State<App>, req: Request, next: Next) -> Response {
	// `/healthz` touches nothing, so it is exempt by path (this layer wraps `auth` and cannot be
	// mounted inside the health routes). `/readyz` is not free — it reads `db_version()` off the
	// reader pool, so a flood starved every request into `E-CORE-UNAVAILABLE`.
	let scope = match req.uri().path() {
		"/healthz" => return next.run(req).await,
		"/readyz" => "readyz",
		// Not in `SCOPES`, so `check` falls through to `settings['ratelimit.default']`.
		_ => "default",
	};
	let Some(ClientIp(ip)) = req.extensions().get::<ClientIp>().copied() else {
		warn_missing_client_ip();
		return next.run(req).await;
	};
	if let Err(e) = app.limits.check(&app.settings, scope, &bucket_key(ip)).await {
		return e.into_response();
	}
	next.run(req).await
}

/// The rate-limit identity of a client address. IPv4 is the address; IPv6 is masked to its
/// /64, because a single routed allocation hands one client 2^64 addresses and every per-IP
/// bucket would otherwise be a fresh bucket per request — `register` (3/h), `pow` (60/min)
/// and `login.ip` (10/5min) were all bypassable at line rate over IPv6.
///
/// Only the *bucket key* is masked. `Ctx::ip` and the audit trail keep the full address.
pub fn bucket_key(ip: IpAddr) -> String {
	match ip {
		IpAddr::V4(v4) => v4.to_string(),
		IpAddr::V6(v6) => {
			let mut octets = v6.octets();
			octets[8..].fill(0);
			format!("{}/64", Ipv6Addr::from(octets))
		}
	}
}

/// The shipped limits for the abuse-facing scopes.
///
/// [`RateLimiter::check`] resolves in this order: an explicitly set `ratelimit.{scope}` row,
/// then this table, then `settings['ratelimit.default']`. The first step goes through
/// [`Settings::row`] rather than `text`, because `ratelimit.` is a prefix family whose
/// default would otherwise answer for every unset key and silently *loosen* every entry
/// below — `login.ip` would go from `10/5min` to `120/min`.
pub const SCOPES: &[(&str, &str)] = &[
	("register", "3/h/ip"),
	("login.ip", "10/5min/ip"),
	// The per-account half, covering one address attacked from many IPs (NIST SP 800-63B §5.2.2).
	// **Counts but never denies**: the key is the request-body address, so a hard deny is a
	// lockout anyone could aim at a victim. `Auth::login` escalates to proof-of-work, and charges
	// the bucket itself because a layer would have to buffer and parse the body to key on it.
	("login.email", "10/5min/email"),
	// Six digits and `check_code` accepts three time steps, so the same tight budget as the
	// password. Keyed on IP because the route is public and its ticket is unreadable from a
	// layer; the per-account half is `login.totp.account` below.
	("login.totp", "5/5min/ip"),
	// Split out of `login.email`: draining that attacker-supplied-key bucket at the password
	// stage locked a 2FA user out at the *TOTP* step. Every call site here needs a valid ticket,
	// reset token or session, so a hard deny is safe.
	("login.totp.account", "5/5min/account"),
	// The reset path's own half, not `login.totp.account`'s: sharing let someone holding the
	// password drain it at `POST /api/auth/login/totp` and block the victim's own reset.
	("reset.totp.account", "5/5min/account"),
	// Step-up takes the same two credentials as login, so it gets the same budget — an
	// unlimited one would be the cheapest place to brute-force either of them.
	("step_up", "5/5min/account"),
	// Inviting mails a stranger, so it is budgeted — but not out of `register`'s 3/h/ip, which it
	// used to share: the layer runs before `add_member`'s `admin_of` check, so three unauthorized
	// POSTs 429'd registration for everyone behind that address.
	("invite", "30/h/ip"),
	("pow", "60/min/ip"),
	// Keyed on IP for the same reason as `login.totp`: the address lives in the request body,
	// which a layer would have to buffer to read.
	("password_reset", "3/h/ip"),
	// A whole-database read per call, and nothing a person does more than a handful of times
	// a year — so it gets the export-abuse budget rather than `ratelimit.default`.
	("account_export", "3/h/account"),
	// The authenticated tier: one budget covering every call an account makes. Same number as
	// `ratelimit.default` deliberately — the blanket tier already caps one *address* at 120,
	// and this is what an account rotating source addresses cannot escape.
	(AUTHENTICATED, "120/min/account"),
	// Loose enough for a NAT full of just-expired access tokens, far tighter than the blanket
	// 120/min. The proof-of-work gate reads this bucket, so this number is also the ceiling on
	// `settings['auth.pow_after_failures']` — a threshold above it can never fire.
	(AUTH_FAILED, "20/5min/ip"),
	// Readiness hits the reader pool, so unlike `/healthz` it is not free. Generous enough for
	// a per-second probe from several orchestrator addresses, tight enough that a flood cannot
	// starve the 5-connection reader pool out from under every authenticated request.
	("readyz", "120/min/ip"),
	// `GET /api/legal/{kind}` is unauthenticated and reads the document body straight off the
	// reader pool — `LEGAL_DOCS` caches only `(version, sha256)` — and a body may be 4 MB.
	("legal", "30/min/ip"),
	("refs.preview", "30/min/ip"),
	// Fires on every login page view, not on a login: conditional UI asks for the challenge
	// before the user acts, so sharing `login.ip`'s 10/5min would 429 an office NAT's login page.
	("wa.challenge", "60/min/ip"),
	// A QR login is an interactive act a person performs deliberately, not a page load, so this
	// is `login.ip`'s budget rather than `wa.challenge`'s — and each init mints a session that
	// lives two minutes.
	("qr.init", "10/5min/ip"),
	// The approving phone reads this once per scan. Keyed on the account, not the address: the
	// caller is authenticated here, unlike `qr.init`, whose caller has no account yet.
	("qr.details", "30/min/account"),
	// `POST /api/webhook/{provider}` is public and the gateway retries what it cannot deliver,
	// so the bucket has to clear a burst from one gateway's addresses rather than turn a busy
	// hour into a retry storm.
	("webhook", "600/min/ip"),
];

/// The scope every authenticated call charges, keyed on `Claims.sub` (the `accounts.uid`).
/// Mount it as `from_fn_with_state(ratelimit::AUTHENTICATED, scoped_account_mw)` *inside*
/// `auth_mw::require_auth`, so the `Claims` it keys on are already in the extensions.
pub const AUTHENTICATED: &str = "authenticated";

/// The scope [`require_auth`](crate::auth_mw::require_auth) charges when it rejects — and
/// [`optional_auth`](crate::auth_mw::optional_auth) when the handler behind it answers 401,
/// which is how a wrong password on the public `POST /api/auth/login` is counted. Keyed on
/// [`bucket_key`]. A named constant because `mintworks_auth`'s proof-of-work gate reads `consumed`
/// on this very bucket, and a typo there answers zero forever instead of failing.
pub const AUTH_FAILED: &str = "auth.failed";

/// Charges `scope` against the caller's IP for one route. Layer it on the `MethodRouter`:
/// `post(register::register).layer(from_fn_with_state("register", scoped_ip_mw))`.
///
/// The scope rides in as the middleware's state because a `Router<App>` is built before any
/// `App` exists; the `App` itself comes from the extensions, as in `auth_mw::authenticate`.
/// A request with no resolved [`ClientIp`] passes through, exactly as in [`default_mw`].
pub async fn scoped_ip_mw(State(scope): State<&'static str>, req: Request, next: Next) -> Response {
	let Some(app) = req.extensions().get::<App>().cloned() else {
		return Error::internal("rate-limit middleware mounted without the App extension")
			.into_response();
	};
	let Some(ClientIp(ip)) = req.extensions().get::<ClientIp>().copied() else {
		warn_missing_client_ip();
		return next.run(req).await;
	};
	if let Err(e) = app.limits.check(&app.settings, scope, &bucket_key(ip)).await {
		return e.into_response();
	}
	next.run(req).await
}

/// Charges `scope` against the caller's account uid for one route. Layer it on the
/// `MethodRouter` *inside* `auth_mw::require_auth`, which is what inserts the `Claims`.
pub async fn scoped_account_mw(
	State(scope): State<&'static str>,
	req: Request,
	next: Next,
) -> Response {
	let Some(app) = req.extensions().get::<App>().cloned() else {
		return Error::internal("rate-limit middleware mounted without the App extension")
			.into_response();
	};
	let Some(sub) = req.extensions().get::<Claims>().map(|c| c.sub.clone()) else {
		return Error::internal("scoped_account_mw mounted outside require_auth").into_response();
	};
	if let Err(e) = app.limits.check(&app.settings, scope, &sub).await {
		return e.into_response();
	}
	next.run(req).await
}

/// Charges the auth-failed bucket and answers `denial` — or the 429 that replaces it once the
/// bucket is empty. `auth_mw::authenticate` calls this on every rejection from a required
/// bundle; an off-socket call has no key and is passed through unchanged.
pub async fn charge_auth_failure(app: &App, ip: Option<IpAddr>, denial: Response) -> Response {
	let Some(ip) = ip else { return denial };
	match app.limits.check(&app.settings, AUTH_FAILED, &bucket_key(ip)).await {
		Ok(()) => denial,
		Err(e) => e.into_response(),
	}
}

/// The map size at which the first sweep runs, and the floor every later threshold keeps.
/// The blanket tier every scope not in [`SCOPES`] falls back to — which `default_mw` makes
/// every route in the process.
const DEFAULT_SCOPE: &str = "ratelimit.default";

const SWEEP_AT: usize = 4096;

/// Hard ceiling on live buckets. The `retain` in `take` drops only buckets refilled to `max`,
/// which bounds the map by keys *in debt* — fine at 120/min, useless at `register`'s 3/h,
/// where one token is 20 minutes of refill and a fresh IPv6 /64 per request parks a resident
/// bucket for each.
const MAX_BUCKETS: usize = 100_000;

/// Token scale: continuous refill needs sub-token resolution. Milli-tokens rather than the
/// `f64` this used to be, which let `"nan/min/ip"` parse into a bucket that never denied
/// again — `x < 1.0` is false for NaN.
const MILLI: i64 = 1000;

/// The longest key a bucket is keyed on. The sweep bounds the map by key *count*, never by
/// size, so a request-body-keyed scope turned axum's 2 MB body limit into a 2 MB map key.
/// Truncation can only merge keys already too long to be a valid address.
const MAX_KEY_CHARS: usize = 254;

/// The one place a bucket key is built. `consumed` and `take` must agree, or
/// `note_login_failure`'s write and the PoW gate's read would look at different buckets.
fn map_key(scope: &str, key: &str) -> String {
	let mut out = String::with_capacity(scope.len() + 1 + key.len().min(MAX_KEY_CHARS * 4));
	out.push_str(scope);
	out.push('\u{1}');
	// `chars().take`, not a byte slice: slicing can split a UTF-8 boundary and panic.
	out.extend(key.chars().take(MAX_KEY_CHARS));
	out
}

struct Bucket {
	/// Milli-tokens, capped at `max * MILLI`.
	tokens: i64,
	last: Instant,
	/// Kept per bucket so a sweep can refill it without re-reading its scope's limit.
	/// Whole tokens.
	max: i64,
	window_ms: i64,
}

impl Bucket {
	/// Milli-tokens as of `now`, and the milliseconds of `elapsed` that produced them.
	///
	/// The second half is what `take` needs: integer division truncates, so advancing `last` to
	/// `now` threw away every elapsed span shorter than one token's worth of milliseconds and a
	/// fast-retrying client never refilled at all. See
	/// `a_drained_bucket_refills_under_retries_faster_than_its_quantum`.
	fn refill(&self, now: Instant) -> (i64, u64) {
		let ceiling = i128::from(self.max) * i128::from(MILLI);
		let elapsed_ms =
			i128::try_from(now.duration_since(self.last).as_millis()).unwrap_or(i128::MAX);
		let granted = elapsed_ms.saturating_mul(ceiling) / i128::from(self.window_ms);
		let raw = i128::from(self.tokens).saturating_add(granted);
		// Capped: the surplus is not owed back, so the whole elapsed time is consumed.
		let credited_ms = if raw >= ceiling {
			elapsed_ms
		} else {
			granted.saturating_mul(i128::from(self.window_ms)) / ceiling.max(1)
		};
		(
			i64::try_from(raw.min(ceiling)).unwrap_or(i64::MAX),
			u64::try_from(credited_ms).unwrap_or(u64::MAX),
		)
	}

	/// Milli-tokens as of `now`, without mutating. A sweep needs it read-only.
	fn refilled(&self, now: Instant) -> i64 {
		self.refill(now).0
	}
}

struct Buckets {
	map: HashMap<String, Bucket>,
	/// Map size at which the next sweep runs.
	next_sweep: usize,
	/// …and how many more `take` calls may go by before one runs regardless. Without it
	/// `next_sweep` was a ratchet a drained burst never came back down from — see
	/// `the_sweep_threshold_comes_back_down_after_a_burst`.
	sweep_countdown: usize,
}

impl Default for Buckets {
	fn default() -> Self {
		// Not derived: zeroes would sweep on the very first request.
		Self { map: HashMap::new(), next_sweep: SWEEP_AT, sweep_countdown: SWEEP_AT }
	}
}

#[derive(Default)]
pub struct RateLimiter {
	buckets: Mutex<Buckets>,
}

impl RateLimiter {
	pub fn new() -> Self {
		Self::default()
	}

	/// `scope` names the limit, `key` the caller it is per (an IP, an account uid, an
	/// email). Consumes one token or fails with the seconds until the next one.
	pub async fn check(&self, settings: &Settings, scope: &str, key: &str) -> ClResult<()> {
		let setting = format!("ratelimit.{scope}");
		// Not `Settings::get`, which cannot tell a family default from a configured value.
		let configured = settings.configured(&setting).await?;
		// A malformed value is the operator's fault, not the caller's: propagating it answered
		// every request in the scope `400 E-CORE-SETTING` and rendered operator config into the
		// client's body, 400 being exempt from the 5xx mask. The fallback below gets it too.
		let parsed = configured.and_then(|raw| match parse_limit(&raw) {
			Ok(limit) => Some(limit),
			Err(e) => {
				tracing::error!(setting = %setting, value = %raw, error = %e,
					"malformed rate limit; falling back to the default");
				None
			}
		});
		// The default, never unlimited: failing open on a rate limit is worse than the 400.
		let (max, window_ms) = match parsed {
			Some(limit) => limit,
			None => match SCOPES.iter().find(|(s, _)| *s == scope) {
				Some((_, limit)) => parse_limit(limit)?,
				None => {
					match settings.text(DEFAULT_SCOPE).await.and_then(|raw| parse_limit(&raw)) {
						Ok(limit) => limit,
						Err(e) => {
							tracing::error!(setting = DEFAULT_SCOPE, error = %e,
							"malformed default rate limit; falling back to the registry's");
							parse_limit(settings.registry().definition(DEFAULT_SCOPE)?.default)?
						}
					}
				}
			},
		};
		self.take(scope, key, max, window_ms)
	}

	/// Tokens already spent from `scope`/`key`'s bucket, as of now. Zero for a key that has
	/// never been seen, so an unknown caller and a fresh one are indistinguishable — which is
	/// the point: `mintworks_auth::login` derives its proof-of-work gate from the [`AUTH_FAILED`]
	/// bucket read through this — keyed on the caller's address, not the submitted one —
	/// instead of from `accounts.failed_logins`, which existed only for addresses that exist.
	pub fn consumed(&self, scope: &str, key: &str) -> i64 {
		let now = Instant::now();
		let b = self.buckets.lock();
		let Some(bucket) = b.map.get(&map_key(scope, key)) else { return 0 };
		// Rounded up to a whole token: a caller one milli-token into its second has spent two.
		let spent = (bucket.max.saturating_mul(MILLI) - bucket.refilled(now)).max(0);
		(spent + MILLI - 1) / MILLI
	}

	fn take(&self, scope: &str, key: &str, max: i64, window_ms: i64) -> ClResult<()> {
		let now = Instant::now();
		let b = &mut *self.buckets.lock();
		b.sweep_countdown = b.sweep_countdown.saturating_sub(1);
		if b.map.len() >= b.next_sweep || b.sweep_countdown == 0 {
			// Drops only buckets refilled to `max`, which `or_insert` rebuilds identically, so
			// the map is bounded by keys in *debt*; an age-based sweep went quadratic under the
			// lock. Both sides in milli-tokens — against whole `max` this rebuilt drained
			// buckets full.
			b.map
				.retain(|_, bucket| bucket.refilled(now) < bucket.max.saturating_mul(MILLI));
			// Ranked by debt, never by age: `bucket.last` advances only on refill credit, so a
			// fully drained bucket was the *oldest* entry and got evicted, handing the flooder a
			// full budget back.
			// ponytail: O(n) eviction under the lock; a real LRU only if this shows in a profile.
			if b.map.len() > MAX_BUCKETS {
				// `last` only tie-breaks: a flood arrives at one uniform debt, and ranking on
				// `refilled` alone then evicted nobody.
				let rank = |bucket: &Bucket| (bucket.refilled(now), bucket.last);
				let mut debt: Vec<(i64, Instant)> = b.map.values().map(rank).collect();
				let cut = MAX_BUCKETS / 2;
				debt.select_nth_unstable(cut);
				let least_indebted_kept = debt[cut];
				b.map.retain(|_, bucket| rank(bucket) <= least_indebted_kept);
			}
			// A map entirely in debt drops nothing, so a fixed threshold re-scans under this lock
			// on every later request; doubling makes the scans O(1) amortised.
			b.next_sweep = b.map.len().saturating_mul(2).max(SWEEP_AT);
			b.sweep_countdown = b.next_sweep;
		}
		let bucket = b.map.entry(map_key(scope, key)).or_insert(Bucket {
			tokens: max.saturating_mul(MILLI),
			last: now,
			max,
			window_ms,
		});

		// Re-stamped from the caller so an operator's `ratelimit.*` change applies to a live
		// bucket rather than waiting for it to be swept.
		bucket.max = max;
		bucket.window_ms = window_ms;
		let (tokens, credited_ms) = bucket.refill(now);
		bucket.tokens = tokens;
		// Not `now`: the remainder integer division dropped stays owed to the caller, or a
		// client retrying faster than one token's worth of milliseconds never refills.
		bucket.last += std::time::Duration::from_millis(credited_ms);

		if bucket.tokens < MILLI {
			// Milliseconds until the missing fraction of one token has refilled, rounded up
			// to a whole second — `Retry-After` has no finer unit.
			let missing = i128::from(MILLI - bucket.tokens);
			let wait_ms = missing * i128::from(window_ms) / (i128::from(max) * i128::from(MILLI));
			let wait = u64::try_from((wait_ms + 999) / 1000).unwrap_or(u64::MAX);
			return Err(Error::RateLimit(wait.max(1)));
		}
		bucket.tokens -= MILLI;
		Ok(())
	}
}

/// `"10/5min/ip"` -> `(10, 300_000)`: whole tokens, and the window in milliseconds. The
/// third segment documents what the key is and is ignored — the caller already chose it.
pub(crate) fn parse_limit(raw: &str) -> ClResult<(i64, i64)> {
	let bad = || Error::Setting(format!("malformed rate limit {raw:?}"));
	let mut parts = raw.split('/');
	// Integers, so `"nan"` and `"inf"` fail here rather than parsing into a bucket that never
	// denies again: every comparison against NaN is false, so an `f64` max needs a separate
	// `is_finite()` guard to catch what this parse rejects outright.
	let max: i64 = parts.next().and_then(|s| s.trim().parse().ok()).ok_or_else(bad)?;
	let window = parts.next().ok_or_else(bad)?.trim();
	let split = window.find(|c: char| !c.is_ascii_digit()).unwrap_or(window.len());
	let (count, unit) = window.split_at(split);
	let count: i64 = if count.is_empty() { 1 } else { count.parse().map_err(|_| bad())? };
	let unit_ms: i64 = match unit {
		"s" | "sec" => 1_000,
		"m" | "min" => 60_000,
		"h" | "hour" => 3_600_000,
		"d" | "day" => 86_400_000,
		_ => return Err(bad()),
	};
	// Capped, because the window is `refill`'s divisor: `"1/99999999999999d/ip"` saturated to
	// `i64::MAX` ms and credited zero tokens for any elapsed time a process will ever see, so
	// the scope denied everything until restart. Thirty days is past every documented limit.
	let max_window_ms: i64 = 30 * 86_400_000;
	let window_ms = count.saturating_mul(unit_ms);
	if max < 1 || count < 1 || window_ms > max_window_ms {
		return Err(bad());
	}
	Ok((max, window_ms))
}

/// [`parse_limit`] as a `SettingDef::check` hook, so a malformed `ratelimit.*` is refused when
/// the operator writes it rather than on every request in that scope afterwards.
pub(crate) fn check_limit(raw: &str) -> ClResult<()> {
	parse_limit(raw).map(|_| ())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_the_documented_limits() {
		assert_eq!(parse_limit("120/min/ip").unwrap(), (120, 60_000));
		assert_eq!(parse_limit("10/5min/ip").unwrap(), (10, 300_000));
		assert_eq!(parse_limit("3/h/email").unwrap(), (3, 3_600_000));
		assert_eq!(parse_limit("600/min/ip").unwrap(), (600, 60_000));
		assert!(parse_limit("3/fortnight/ip").is_err());
		assert!(parse_limit("nope").is_err());
		assert!(parse_limit("0/min/ip").is_err());
		// A non-finite max compares false against every bound, so under the old `f64` parse
		// it slipped past the bounds check and turned the bucket off for good. Integers
		// reject it at the parse, but it stays pinned: it is the reason for the conversion.
		for off in ["nan/min/ip", "NaN/min/ip", "inf/min/ip", "-inf/min/ip", "1/nanmin/ip"] {
			assert!(parse_limit(off).is_err(), "{off}");
		}
		// Fractions are gone with the floats; a bucket has to be a whole number of tokens.
		assert!(parse_limit("1.5/min/ip").is_err());
		// A window past the 30-day ceiling saturates `window_ms` and makes `refill` credit
		// nothing, which bricks the scope until restart. Refused at the operator instead.
		assert!(parse_limit("1/99999999999999d/ip").is_err());
		assert!(parse_limit("1/31d/ip").is_err());
		assert_eq!(parse_limit("1/30d/ip").unwrap(), (1, 30 * 86_400_000));
	}

	/// A drained `3/h` bucket earns one milli-token per 1200 ms, so a client polling every
	/// second credited zero — and `last = now` threw that second away, holding the bucket at
	/// zero for as long as the polling continued. `password_reset` is `3/h` keyed on the IP —
	/// but `login.email` is keyed on the request-body address, where that was a lockout anyone
	/// could aim at any account.
	///
	/// Drives `Bucket::refill` directly: `take` reads `Instant::now()` itself.
	#[test]
	fn a_drained_bucket_refills_under_retries_faster_than_its_quantum() {
		let (max, window_ms) = parse_limit("3/h/email").unwrap();
		let start = Instant::now();
		let mut bucket = Bucket { tokens: 0, last: start, max, window_ms };

		// One whole token is 1_200_000 ms at 3/h, so a poll every second reaches it on the
		// 1200th — never, before the fix, however long the client kept polling.
		for i in 1..=1300 {
			let now = start + std::time::Duration::from_secs(i);
			let (tokens, credited_ms) = bucket.refill(now);
			bucket.tokens = tokens;
			bucket.last += std::time::Duration::from_millis(credited_ms);
			assert!(bucket.last <= now, "credited more time than elapsed at poll {i}");
			// Two seconds is the first multiple of the 1200 ms quantum: the credit starts
			// accruing there rather than being discarded.
			assert_eq!(bucket.tokens > 0, i >= 2, "at poll {i}: {} milli-tokens", bucket.tokens);
			if bucket.tokens >= MILLI {
				assert_eq!(i, 1200, "a whole token is 1_200_000 ms at 3/h");
				return;
			}
		}
		panic!("the bucket never refilled: tokens {}", bucket.tokens);
	}

	/// A routed IPv6 allocation hands one client 2^64 source addresses, so keying the bucket
	/// on the full address made every per-IP limit a fresh bucket per request.
	#[test]
	fn ipv6_buckets_on_the_64_and_ipv4_on_the_address() {
		let key = |s: &str| bucket_key(s.parse().unwrap());
		assert_eq!(key("2001:db8::1"), key("2001:db8::dead:beef"));
		assert_ne!(key("2001:db8::1"), key("2001:db8:0:1::1"));
		assert_ne!(key("1.2.3.4"), key("1.2.3.5"));
		assert_eq!(key("1.2.3.4"), "1.2.3.4");
	}

	/// `consumed` is what the login PoW gate reads instead of `accounts.failed_logins`, so a
	/// key nobody has touched must be indistinguishable from a fresh one.
	#[test]
	fn consumed_counts_spent_tokens_and_is_zero_for_an_unseen_key() {
		let rl = RateLimiter::new();
		assert_eq!(rl.consumed("login.ip", "nobody@example.com"), 0);
		for n in 1..=3 {
			rl.take("login.ip", "a@example.com", 5, 300_000).unwrap();
			assert_eq!(rl.consumed("login.ip", "a@example.com"), n);
		}
		assert_eq!(rl.consumed("login.ip", "nobody@example.com"), 0);
	}

	#[test]
	fn bucket_drains_then_reports_a_retry_after() {
		let rl = RateLimiter::new();
		for _ in 0..3 {
			rl.take("register", "1.2.3.4", 3, 3_600_000).unwrap();
		}
		match rl.take("register", "1.2.3.4", 3, 3_600_000) {
			Err(Error::RateLimit(secs)) => assert!((1..=1200).contains(&secs), "{secs}"),
			other => panic!("expected RateLimit, got {other:?}"),
		}
		// A different key has its own bucket.
		rl.take("register", "5.6.7.8", 3, 3_600_000).unwrap();
	}

	/// The keys are attacker-supplied (`login.email` buckets on a request-body address), so
	/// the map has to be bounded by keys currently in debt, not by how long ago each was
	/// touched — otherwise a single IP grows it without limit and every request past
	/// `SWEEP_AT` scans the whole thing under the lock.
	#[test]
	fn a_flood_of_distinct_keys_stays_bounded() {
		let rl = RateLimiter::new();
		// A one-millisecond window, then a pause, so every bucket has genuinely refilled by the
		// time the sweep reaches it without an hour of test time. The fill stops one key short
		// of `SWEEP_AT` so the sweep fires *after* the pause, not during the fill.
		for i in 0..SWEEP_AT {
			rl.take("password_reset", &format!("{i}@example.com"), 3, 1).unwrap();
		}
		std::thread::sleep(std::time::Duration::from_millis(5));
		for i in SWEEP_AT..SWEEP_AT + 100 {
			rl.take("password_reset", &format!("{i}@example.com"), 3, 1).unwrap();
		}
		// The sweep dropped everything that had refilled; only what arrived after it is
		// still held. Age-based eviction kept all of these for an hour each.
		assert!(rl.buckets.lock().map.len() <= SWEEP_AT, "{}", rl.buckets.lock().map.len());
	}

	/// The counterpart: eviction must not hand a throttled caller a fresh full bucket. Two
	/// bugs, one fixture. Fully drained: the sweep evicted the bucket outright. Partly spent:
	/// the sweep compared milli-tokens against whole ones, so anything above `max / 1000` of a
	/// token counted as fully refilled and was rebuilt at full budget by `or_insert`.
	#[test]
	fn a_key_still_in_debt_survives_a_sweep() {
		for spent_before in [3, 1] {
			let rl = RateLimiter::new();
			for _ in 0..spent_before {
				rl.take("password_reset", "victim@example.com", 3, 3_600_000).unwrap();
			}

			for i in 0..SWEEP_AT + 100 {
				rl.take("password_reset", &format!("{i}@example.com"), 3, 3_600_000).unwrap();
			}
			// Whatever was left before the sweep is still all that is left after it.
			for _ in spent_before..3 {
				rl.take("password_reset", "victim@example.com", 3, 3_600_000).unwrap();
			}
			assert!(
				rl.take("password_reset", "victim@example.com", 3, 3_600_000).is_err(),
				"a bucket {spent_before}/3 spent was swept away and the caller got it back"
			);
		}
	}

	/// The `MAX_BUCKETS` backstop used to keep the newest buckets by `bucket.last`, which only
	/// advances on refill credit — so the most-throttled key was the oldest and was evicted
	/// first, and flooding distinct keys reset a 3/h throttle.
	#[test]
	fn the_most_throttled_key_survives_the_max_buckets_eviction() {
		let rl = RateLimiter::new();
		for _ in 0..3 {
			rl.take("password_reset", "victim@example.com", 3, 3_600_000).unwrap();
		}
		assert!(rl.take("password_reset", "victim@example.com", 3, 3_600_000).is_err());

		// Every filler key is in debt too, so `retain` drops nothing and only the backstop can
		// shrink the map — which is the branch under test.
		let mut evicted = false;
		for i in 0..4 * MAX_BUCKETS {
			rl.take("password_reset", &format!("{i}@example.com"), 3, 3_600_000).unwrap();
			if rl.buckets.lock().map.len() <= i {
				evicted = true;
				break;
			}
		}
		assert!(evicted, "the MAX_BUCKETS backstop never ran");
		assert!(
			rl.take("password_reset", "victim@example.com", 3, 3_600_000).is_err(),
			"the most-throttled key was evicted and the caller got a fresh budget"
		);
	}

	/// The sweep only drops buckets that have refilled, so a map that is entirely in debt
	/// drops nothing — and the old `len() > SWEEP_AT` test then re-ran the full O(n) scan
	/// under the global lock on *every* subsequent request.
	#[test]
	fn a_sweep_that_drops_nothing_does_not_re_scan_on_the_next_request() {
		let rl = RateLimiter::new();
		// A one-token-per-hour budget, spent immediately: every key stays in debt.
		for i in 0..SWEEP_AT + 100 {
			rl.take("password_reset", &format!("{i}@example.com"), 1, 3_600_000).unwrap();
		}
		let b = rl.buckets.lock();
		assert!(b.next_sweep > b.map.len(), "next_sweep {} <= len {}", b.next_sweep, b.map.len());
	}

	/// And that doubling must not be permanent. `next_sweep` was derived from the
	/// post-retain size and only ever grew, so once a burst that had retained everything
	/// drained, `len >= next_sweep` was false forever and the dead buckets stayed resident
	/// until that many *new* distinct keys arrived. `sweep_countdown` is what makes a second
	/// sweep reachable when the map has not grown.
	#[test]
	fn the_sweep_threshold_comes_back_down_after_a_burst() {
		const WINDOW_MS: i64 = 400;
		let rl = RateLimiter::new();
		// One token per key over a window long enough that the whole fill is over before
		// anything refills: the sweep the fill triggers therefore retains every key and
		// ratchets the threshold up, which is the state the bug needed.
		for i in 0..SWEEP_AT + 100 {
			rl.take("password_reset", &format!("{i}@example.com"), 1, WINDOW_MS).unwrap();
		}
		let ratcheted = rl.buckets.lock().next_sweep;
		assert!(ratcheted > SWEEP_AT, "the burst should have raised next_sweep: {ratcheted}");

		// Everything from the burst has refilled and is now droppable.
		std::thread::sleep(std::time::Duration::from_millis(WINDOW_MS as u64 + 100));

		// A single key on a budget it cannot exhaust: the map does not grow, so only the
		// countdown can bring the sweep back.
		for _ in 0..ratcheted {
			rl.take("password_reset", "survivor@example.com", 1_000_000, 3_600_000).unwrap();
		}
		let b = rl.buckets.lock();
		assert_eq!(b.map.len(), 1, "the burst's dead buckets were never reclaimed");
		assert_eq!(b.next_sweep, SWEEP_AT, "next_sweep did not decay");
	}

	/// The second factor is six digits and accepted across three time steps, so an
	/// unbudgeted `POST /api/auth/login/totp` is brute-forceable at line rate. It gets a
	/// budget no looser than the password's, keyed on IP — the ticket naming the account is
	/// in the request body, which a layer cannot read.
	#[test]
	fn the_second_factor_has_a_budget_as_tight_as_the_password() {
		let totp = SCOPES.iter().find(|(s, _)| *s == "login.totp").map(|(_, l)| *l);
		assert_eq!(totp, Some("5/5min/ip"));
		let password = SCOPES.iter().find(|(s, _)| *s == "login.ip").map(|(_, l)| *l);

		let (burst, window_ms) = parse_limit(totp.unwrap()).unwrap();
		let (pw_burst, pw_window) = parse_limit(password.unwrap()).unwrap();
		assert!(burst <= pw_burst && window_ms >= pw_window, "looser than the password's");

		let rl = RateLimiter::new();
		for _ in 0..5 {
			rl.take("login.totp", "1.2.3.4", burst, window_ms).unwrap();
		}
		assert!(rl.take("login.totp", "1.2.3.4", burst, window_ms).is_err());
	}

	/// No framework scope keys on the raw request body any more — every one of them is an
	/// IP or an account uid — but `check` takes an arbitrary `key`, so a consumer's own scope
	/// can still be handed one. The sweep bounds the map by key *count*, so without the cap a
	/// flood parked megabyte-sized keys in resident memory for the whole window.
	#[test]
	fn a_bucket_key_is_bounded_in_size_and_still_agrees_with_itself() {
		let long = "a".repeat(1024 * 1024);
		let rl = RateLimiter::new();
		rl.take(AUTH_FAILED, &long, 5, 60_000).unwrap();

		let stored = rl.buckets.lock().map.keys().next().unwrap().clone();
		assert!(stored.len() <= AUTH_FAILED.len() + 1 + MAX_KEY_CHARS, "{}", stored.len());

		// `consumed` and `take` must build the same key, or the middleware's charge and the
		// PoW gate's read would look at different buckets.
		assert_eq!(rl.consumed(AUTH_FAILED, &long), 1);

		// Two keys differing only past the cap share a bucket. Both are already far longer
		// than anything a caller legitimately produces, so the merge costs nothing.
		let other = format!("{long}-and-then-some");
		assert_eq!(rl.consumed(AUTH_FAILED, &other), 1);
		assert_eq!(rl.buckets.lock().map.len(), 1);

		// A multi-byte key truncates on a character boundary rather than panicking.
		rl.take(AUTH_FAILED, &"é".repeat(1024), 5, 60_000).unwrap();
	}
}

// vim: ts=4
