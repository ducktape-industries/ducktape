//! Commands for the requesting guest's own host window. No native window ID crosses.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WindowCommand {
    Focus,
    Maximize(bool),
    Minimize(bool),
    Resizable(bool),
    Resize { width: f32, height: f32 },
    Close,
}

impl WindowCommand {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::Resize { width, height } = *self
            && crate::manifest::PreferredSize::new(width, height).is_none()
        {
            return Err("RequestError: window size must satisfy the preferred-size bounds".into());
        }
        Ok(())
    }

    /// Fixed-size payload, rejecting trailing bytes before any host side effect.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > 12 {
            return Err("RequestError: window command exceeds 12 bytes".into());
        }
        let command: Self = crate::decode(bytes)
            .map_err(|error| format!("RequestError: invalid window command: {error}"))?;
        if crate::encoded_size(&command) != bytes.len() as u64 {
            return Err("RequestError: trailing window command bytes".into());
        }
        command.validate()?;
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_commands_reject_invalid_sizes_and_extra_payload() {
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, 8193.0] {
            assert!(
                WindowCommand::decode(&crate::encode(&WindowCommand::Resize {
                    width: size,
                    height: 400.0
                }))
                .is_err()
            );
        }
        let valid = WindowCommand::Resize {
            width: 600.5,
            height: 400.25,
        };
        assert_eq!(
            WindowCommand::decode(&crate::encode(&valid)).unwrap(),
            valid
        );
        let mut extra = crate::encode(&WindowCommand::Close);
        extra.push(0);
        assert!(WindowCommand::decode(&extra).is_err());
        assert!(WindowCommand::decode(&[255; 4]).is_err());
    }
}
