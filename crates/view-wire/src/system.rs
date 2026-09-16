//! Host OS facts, distinct from a guest's selected application palette.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    None,
    Light,
    Dark,
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn from_name(name: &str) -> Result<Self, &'static str> {
        match name {
            "none" => Ok(Self::None),
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            _ => Err("invalid system theme"),
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 4 {
            return Err("system theme reply must be four bytes".into());
        }
        crate::decode(bytes).map_err(|error| format!("invalid system theme reply: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_replies_preserve_none_and_reject_unknown_or_trailing_data() {
        for theme in [Theme::None, Theme::Light, Theme::Dark] {
            assert_eq!(Theme::from_name(theme.name()), Ok(theme));
            let mut bytes = crate::encode(&theme);
            assert_eq!(Theme::decode(&bytes), Ok(theme));
            bytes.push(0);
            assert!(Theme::decode(&bytes).is_err());
        }
        assert!(Theme::from_name("auto").is_err());
        assert!(Theme::decode(&[255; 4]).is_err());
        assert!(Theme::decode(&[]).is_err());
    }
}
