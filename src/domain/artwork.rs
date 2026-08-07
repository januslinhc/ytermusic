use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use url::Url;

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ArtworkUrl(Url);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ArtworkUrlError {
    #[error("artwork URL must use HTTP or HTTPS and include a host")]
    Invalid,
}

impl ArtworkUrl {
    #[must_use]
    pub const fn as_url(&self) -> &Url {
        &self.0
    }
}

impl TryFrom<Url> for ArtworkUrl {
    type Error = ArtworkUrlError;

    fn try_from(url: Url) -> Result<Self, Self::Error> {
        if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
            Ok(Self(url))
        } else {
            Err(ArtworkUrlError::Invalid)
        }
    }
}

impl fmt::Debug for ArtworkUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ArtworkUrl([REDACTED])")
    }
}

impl fmt::Display for ArtworkUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED artwork URL]")
    }
}

impl Serialize for ArtworkUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ArtworkUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let url = Url::deserialize(deserializer)?;
        Self::try_from(url).map_err(serde::de::Error::custom)
    }
}
