// SPDX-License-Identifier: MPL-2.0
//! The `E-SCRIPT-*` codes. An `E-APP-*` code is minted by `api::err::app`, which interns it.

use mintworks_core::error::{Error, StatusCode};

use crate::value::ScriptError;

pub(crate) type R<T> = Result<T, ScriptError>;

/// A 400 on the script's own argument.
pub(crate) fn bad(msg: impl Into<String>) -> ScriptError {
	ScriptError(Error::validation(msg))
}

/// A source set did not compile, or a bundle declared something the runtime rejects.
pub const E_COMPILE: &str = "E-SCRIPT-COMPILE";
/// The invocation exhausted its instruction budget.
pub const E_BUDGET: &str = "E-SCRIPT-BUDGET";
/// The invocation ran past its wall-clock deadline.
pub const E_TIMEOUT: &str = "E-SCRIPT-TIMEOUT";
/// Any other Rune failure: a trap, a bad argument, a value the host cannot read.
pub const E_RUNTIME: &str = "E-SCRIPT-RUNTIME";
/// Direct SQL refused: no `AppDb` registered, a statement keyword outside the allowlist, an
/// argument that cannot bind, a column type that cannot cross into script, a nested `db::tx`, or
/// a `db::exec`/`db::tx` inside `tx::with`.
pub const E_DB: &str = "E-SCRIPT-DB";
/// A script route's body was not received within the read deadline.
pub const E_BODY_TIMEOUT: &str = "E-SCRIPT-BODY-TIMEOUT";
/// A script route's body exceeds its `.max_body()` (capped by `script.max_body_bytes`).
pub const E_TOOLARGE: &str = "E-SCRIPT-TOOLARGE";

/// All five are 5xx: a broken or runaway bundle is an operator fault, and the detail is
/// logged rather than returned.
pub fn compile(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::INTERNAL_SERVER_ERROR, E_COMPILE, msg)
}

pub fn budget(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::INTERNAL_SERVER_ERROR, E_BUDGET, msg)
}

pub fn timeout(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::INTERNAL_SERVER_ERROR, E_TIMEOUT, msg)
}

pub fn runtime(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::INTERNAL_SERVER_ERROR, E_RUNTIME, msg)
}

pub fn db(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::INTERNAL_SERVER_ERROR, E_DB, msg)
}

/// The client's fault, so 4xx with the message returned.
pub fn body_timeout(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::REQUEST_TIMEOUT, E_BODY_TIMEOUT, msg)
}

pub fn toolarge(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::PAYLOAD_TOO_LARGE, E_TOOLARGE, msg)
}

// vim: ts=4
