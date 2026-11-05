//! Rex's session and block ids. They are both opaque strings on the wire; separate types keep
//! one from being passed where the other belongs.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                $name(id)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                $name(id.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id! {
    /// A session id, such as `session:01a1…`.
    SessionId
}

id! {
    /// A block id, such as `block:01a1…`.
    BlockId
}
