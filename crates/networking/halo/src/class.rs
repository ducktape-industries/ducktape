//! The priority a halo stream is served at, and the cap on what an asker may claim.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    Critical,
    Member,
    Client,
}

impl Class {
    pub const COUNT: usize = 3;
    pub const ALL: [Class; Class::COUNT] = [Class::Critical, Class::Member, Class::Client];

    pub fn rank(self) -> usize {
        self as usize
    }

    pub fn capped(self, validator: bool) -> Class {
        let claims_critical = self == Class::Critical;
        if claims_critical && !validator {
            return Class::Member;
        }
        self
    }
}

impl From<Class> for u8 {
    fn from(class: Class) -> u8 {
        class as u8
    }
}

impl TryFrom<u8> for Class {
    type Error = u8;

    fn try_from(byte: u8) -> Result<Class, u8> {
        Class::ALL.get(usize::from(byte)).copied().ok_or(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_validator_is_served_at_critical() {
        assert_eq!(Class::Critical.capped(true), Class::Critical);
        assert_eq!(Class::Critical.capped(false), Class::Member);
        for class in [Class::Member, Class::Client] {
            assert_eq!(class.capped(false), class);
            assert_eq!(class.capped(true), class);
        }
    }

    #[test]
    fn a_class_survives_its_byte() {
        for class in Class::ALL {
            assert_eq!(Class::try_from(u8::from(class)), Ok(class));
        }
        assert_eq!(Class::try_from(3), Err(3));
    }
}
