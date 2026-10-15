//! Text names of the settings enums, for flags, ROS parameters, and logs.

/// Implement `Display` (each variant's first name) and case-insensitive `FromStr` (any of its
/// names) for a fieldless enum. Unknown names fail with [`Error::InvalidSettings`](crate::Error),
/// listing the valid ones.
macro_rules! named_enum {
	($ty:ty, $what:literal, { $($variant:ident => $name:literal $(| $alias:literal)*),+ $(,)? }) => {
		impl std::fmt::Display for $ty {
			fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				f.write_str(match self {
					$(Self::$variant => $name,)+
				})
			}
		}

		impl std::str::FromStr for $ty {
			type Err = crate::Error;

			fn from_str(s: &str) -> crate::Result<Self> {
				match s.to_ascii_lowercase().as_str() {
					$($name $(| $alias)* => Ok(Self::$variant),)+
					other => Err(crate::Error::InvalidSettings(format!(
						concat!("unknown ", $what, " {:?} (expected one of: {})"),
						other,
						[$($name),+].join(", "),
					))),
				}
			}
		}
	};
}

pub(crate) use named_enum;

#[cfg(test)]
mod tests {
	#[derive(Debug, PartialEq)]
	enum Fruit {
		Apple,
		Pear,
	}

	super::named_enum!(Fruit, "fruit", { Apple => "apple" | "malus", Pear => "pear" });

	#[test]
	fn names_round_trip() {
		assert_eq!(Fruit::Apple.to_string(), "apple");
		assert_eq!("PEAR".parse::<Fruit>().unwrap(), Fruit::Pear);
		assert_eq!("malus".parse::<Fruit>().unwrap(), Fruit::Apple);
		let err = "fig".parse::<Fruit>().unwrap_err().to_string();
		assert_eq!(
			err,
			r#"invalid settings: unknown fruit "fig" (expected one of: apple, pear)"#
		);
	}
}
