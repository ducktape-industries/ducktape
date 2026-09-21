//! The framing a module refusal crosses the component boundary in.
//!
//! The `ducktape:module` world carries a refusal as ONE string
//! (`error.rejected(string)`), and it stays that way on purpose: a record
//! would move the world, and moving the world forces every node to upgrade in
//! lockstep before a single new guest can be swapped onto a live pair. A
//! framing over the string lets a module ship new tokens as a wasm-only
//! update.
//!
//! Both sides of that boundary link this crate, so the pair lives here once:
//! `crates/module-sdk`'s adapter encodes on the guest side and decodes what a
//! sibling read answers, and `wasm-host` does the mirror.
//!
//! The frame is `"<reason>: <sentence>"` — a non-empty snake_case token, the
//! two-character separator, then the sentence VERBATIM. [`decode`] splits on
//! the FIRST separator, so a sentence that itself contains `": "` survives the
//! round trip whole.
//!
//! The tokens themselves, and the rule for minting one, live in
//! [`refusal_class`]; every constant there is re-exported here, so a module
//! writes `sdk::refusal::STALE`.

pub use refusal_class::*;

/// frame a refusal as the one string the world's `rejected` carries.
pub fn encode(reason: &str, sentence: &str) -> String {
    format!("{reason}: {sentence}")
}

/// split a framed refusal into `(reason, sentence)`.
///
/// `None` when the string is not framed — a peer that did not frame its
/// refusal does not get a token invented for it here. The caller fails closed
/// instead: a made-up word is one every consumer would then have to tell apart
/// from a word a module actually chose.
pub fn decode(framed: &str) -> Option<(&str, &str)> {
    let (reason, sentence) = framed.split_once(": ")?;
    let snake_case = !reason.is_empty()
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    snake_case.then_some((reason, sentence))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sentence_carrying_the_separator_round_trips_whole() {
        let sentence = "store-backed state keys: got 7 bytes: expected 32";
        let framed = encode("state_key_shape", sentence);
        assert_eq!(decode(&framed), Some(("state_key_shape", sentence)));

        // the edges: an empty sentence is a sentence, a trailing separator is
        // part of one.
        assert_eq!(decode(&encode("codec", "")), Some(("codec", "")));
        assert_eq!(decode(&encode("codec", ": ")), Some(("codec", ": ")));
    }

    /// a class the framing cannot carry would fail closed at the boundary.
    #[test]
    fn every_class_crosses_the_frame() {
        for token in [
            NOT_FOUND,
            ALREADY_EXISTS,
            STALE,
            WRONG_STATE,
            INVALID_INPUT,
            CAPACITY,
            NOT_YET,
            EXHAUSTED,
            UNAUTHORIZED,
            UNSUPPORTED,
            CORRUPT,
            UNEXPECTED_REPLY,
            TRAP,
            UNFRAMED_REFUSAL,
        ] {
            assert_eq!(decode(&encode(token, "x")), Some((token, "x")));
        }
    }

    #[test]
    fn an_unframed_string_decodes_to_nothing_rather_than_a_default_token() {
        for unframed in [
            "no separator at all",
            ": leading separator",
            "Not_Snake_Case: sentence",
            "two words: sentence",
            "kebab-case: sentence",
            "codec:no space after the colon",
        ] {
            assert_eq!(decode(unframed), None, "{unframed:?}");
        }
    }
}
