//! The generic bridge between Rune values and `serde_json::Value`, and the error value every
//! fallible host call hands back to script.

use rune::{Any, ContextError, Module, Value, runtime::Object};
use saas_core::error::{ClResult, Error};
use serde::{
	Deserialize,
	ser::{Serialize, SerializeMap, SerializeSeq, Serializer},
};

use crate::{
	error,
	money::{Money, Qty},
};

/// The `Err` payload of every fallible host call.
///
/// Host calls are written `host_call(…)?`, so a failure has to be a Rune
/// `Result` value rather than a VM error. The framework [`Error`] inside carries the status
/// and the `errCode`, so a script's `?` propagates a 404 as a 404 and an `E-APP-*` with the
/// status the bundle declared for it.
#[derive(Debug, Any)]
pub struct ScriptError(pub Error);

impl From<Error> for ScriptError {
	fn from(err: Error) -> Self {
		Self(err)
	}
}

/// Registers [`ScriptError`] so script code can hold and propagate one.
///
/// The `err::` constructor functions are a separate module; this
/// installs the type itself, and must not be installed twice.
///
/// # Errors
/// Whatever Rune raises registering the type.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::new();
	m.ty::<ScriptError>()?;
	Ok(m)
}

/// A Rune value as JSON, with the host money types replaced by their wire shapes.
///
/// # Errors
/// `E-SCRIPT-RUNTIME` for a value with no JSON form — a struct, a type, a function.
pub fn to_json(value: &Value) -> ClResult<serde_json::Value> {
	serde_json::to_value(Bridge(value, 0))
		.map_err(|e| error::runtime(format!("script value is not representable as JSON: {e}")))
}

/// JSON as a Rune value.
///
/// A `Money` is **not** reconstructed on the way in: an amount reaches script as the two
/// strings of its wire shape and becomes opaque only through `money(amount, currency)`, which
/// parses integers. A JSON float stays a Rune float — it can never become an amount, because
/// that is the only door.
///
/// # Errors
/// `E-SCRIPT-RUNTIME` for JSON Rune cannot represent.
pub fn from_json(json: &serde_json::Value) -> ClResult<Value> {
	Value::deserialize(json)
		.map_err(|e| error::runtime(format!("JSON is not representable in Rune: {e}")))
}

/// Rune's own `Serialize for Value` refuses an external reference, which is exactly what a
/// `Money` nested inside a returned object is, so every container is walked to intercept one.
/// Leaf scalars and strings delegate back to Rune, which cannot nest one.
struct Bridge<'a>(&'a Value, u32);

/// Deep enough for any real payload, shallow enough that the recursion cannot reach the guard
/// page: `Bridge` runs after the budget and the deadline are both spent, and a stack overflow
/// aborts the process where a `CatchPanicLayer` panic would not.
const MAX_DEPTH: u32 = 64;

impl Serialize for Bridge<'_> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		if self.1 > MAX_DEPTH {
			return Err(serde::ser::Error::custom("script value nests deeper than 64 levels"));
		}
		if let Ok(money) = self.0.borrow_ref::<Money>() {
			return money.to_wire().serialize(serializer);
		}
		// `Qty`'s own `Serialize` renders the 1e6 scale as a decimal string, never a float.
		if let Ok(qty) = self.0.borrow_ref::<Qty>() {
			return qty.inner().serialize(serializer);
		}
		if let Ok(vec) = self.0.borrow_ref::<rune::runtime::Vec>() {
			let mut seq = serializer.serialize_seq(Some(vec.len()))?;
			for item in vec.iter() {
				seq.serialize_element(&Bridge(item, self.1 + 1))?;
			}
			return seq.end();
		}
		if let Ok(tuple) = self.0.borrow_ref::<rune::runtime::OwnedTuple>() {
			let mut seq = serializer.serialize_seq(Some(tuple.len()))?;
			for item in tuple.iter() {
				seq.serialize_element(&Bridge(item, self.1 + 1))?;
			}
			return seq.end();
		}
		if let Ok(object) = self.0.borrow_ref::<Object>() {
			let mut map = serializer.serialize_map(Some(object.len()))?;
			for (key, value) in object.iter() {
				map.serialize_entry(key, &Bridge(value, self.1 + 1))?;
			}
			return map.end();
		}
		if let Ok(option) = self.0.borrow_ref::<Option<Value>>() {
			return match &*option {
				Some(value) => serializer.serialize_some(&Bridge(value, self.1 + 1)),
				None => serializer.serialize_none(),
			};
		}
		self.0.serialize(serializer)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_nested_structure_round_trips() {
		let json = serde_json::json!({
			"a": [1, "two", {"b": true}],
			"c": null,
			"d": {"e": [[], {}]},
		});
		let value = from_json(&json).unwrap();
		assert_eq!(to_json(&value).unwrap(), json);
	}

	#[test]
	fn money_crosses_out_as_its_wire_shape() {
		let money = Money::new("12500.00", "HUF").unwrap();
		let value = rune::to_value(money).unwrap();
		assert_eq!(
			to_json(&value).unwrap(),
			serde_json::json!({"amount": "12500.00", "currency": "HUF"})
		);
	}

	#[test]
	fn a_nested_money_crosses_out_too() {
		let mut object = Object::new();
		object
			.insert(
				rune::alloc::String::try_from("total").unwrap(),
				rune::to_value(Money::new("1.00", "EUR").unwrap()).unwrap(),
			)
			.unwrap();
		let value = rune::to_value(object).unwrap();
		assert_eq!(
			to_json(&value).unwrap(),
			serde_json::json!({"total": {"amount": "1.00", "currency": "EUR"}})
		);
	}

	#[test]
	fn a_value_nested_past_the_cap_is_an_error_not_an_abort() {
		let mut value = rune::to_value(1i64).unwrap();
		for _ in 0..200 {
			let mut vec = rune::runtime::Vec::new();
			vec.push(value).unwrap();
			value = rune::to_value(vec).unwrap();
		}
		assert!(to_json(&value).is_err());
	}

	#[test]
	fn qty_crosses_out_as_a_decimal_string() {
		let value = rune::to_value(Qty::new("2.5").unwrap()).unwrap();
		assert_eq!(to_json(&value).unwrap(), serde_json::json!("2.500000"));
	}
}

// vim: ts=4
