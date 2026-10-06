// SPDX-License-Identifier: MPL-2.0
//! [`str_enum!`](crate::str_enum!), shared by every crate that stores an enum in a TEXT column.

/// `as_str` and `FromStr` for an enum stored in a TEXT column.
///
/// The tags are the spellings in the migration's `CHECK` constraints and in the type's `serde`
/// renames. That mapping is domain knowledge, so it lives beside the type and the store adapter
/// calls it rather than keeping a table of its own.
///
/// `#[macro_export]` puts this at the crate root: it is `mintworks_core::str_enum!`, not
/// `mintworks_core::str_enum::str_enum!`.
#[macro_export]
macro_rules! str_enum {
	($ty:ident { $($variant:ident => $tag:literal),+ $(,)? }) => {
		impl $ty {
			/// The database and wire spelling of this variant.
			pub fn as_str(self) -> &'static str {
				match self {
					$(Self::$variant => $tag),+
				}
			}
		}

		impl ::std::str::FromStr for $ty {
			type Err = $crate::error::Error;

			/// The input is a column value, so an unknown tag is a corrupt database rather
			/// than bad input, and reports as `Error::Internal`.
			fn from_str(s: &str) -> $crate::error::ClResult<Self> {
				match s {
					$($tag => Ok(Self::$variant),)+
					_ => Err($crate::error::Error::internal(format!(
						concat!("unknown ", stringify!($ty), " in database: {}"),
						s
					))),
				}
			}
		}
	};
}

// vim: ts=4
