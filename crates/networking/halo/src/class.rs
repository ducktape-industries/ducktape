//! The priority a halo stream is served at: what it asked, no higher than its lane's cap, and
//! `Critical` only for a validator.

pub use abi::role::halo::Class;

/// `Critical` is a validator's alone: anyone else claiming it is served at `Member`.
pub(crate) fn claimed(asked: Class, validator: bool) -> Class {
    let claims_critical = asked == Class::Critical;
    if claims_critical && !validator {
        return Class::Member;
    }
    asked
}

/// `asked`, no higher than `cap`. `Ord` puts the higher priority first, so the lower
/// priority of the two is the greater.
pub(crate) fn capped(asked: Class, cap: Class) -> Class {
    asked.max(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_validator_is_served_at_critical() {
        assert_eq!(claimed(Class::Critical, true), Class::Critical);
        assert_eq!(claimed(Class::Critical, false), Class::Member);
        for class in [Class::Member, Class::Client] {
            assert_eq!(claimed(class, false), class);
            assert_eq!(claimed(class, true), class);
        }
    }

    #[test]
    fn a_stream_is_served_at_the_lower_of_what_it_asked_and_its_cap() {
        assert_eq!(capped(Class::Critical, Class::Member), Class::Member);
        assert_eq!(capped(Class::Client, Class::Member), Class::Client);
        for class in Class::ALL {
            assert_eq!(capped(class, Class::Critical), class);
            assert_eq!(capped(class, Class::Client), Class::Client);
        }
    }
}
