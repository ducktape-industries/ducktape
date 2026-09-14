//! Guest-owned combo options. Search and menu state live in the host.
#[derive(Clone, Debug, PartialEq)]
pub struct Combo<T> {
    options: Vec<T>,
    reset: u64,
}
impl<T> Combo<T> {
    pub fn new(options: Vec<T>) -> Self {
        Self { options, reset: 0 }
    }
    pub fn options(&self) -> &[T] {
        &self.options
    }
    pub fn reset_revision(&self) -> u64 {
        self.reset
    }
    pub fn replace(&mut self, options: Vec<T>) {
        self.reset = self
            .reset
            .checked_add(1)
            .expect("combo reset revisions exhausted");
        self.options = options;
    }
    pub fn push(&mut self, value: T) {
        self.options.push(value);
    }
    pub fn into_options(self) -> Vec<T> {
        self.options
    }
    pub fn restore(options: Vec<T>, reset: u64) -> Self {
        Self { options, reset }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacing_identical_options_resets_but_push_preserves_search_revision() {
        let mut combo = Combo::new(vec!["One"]);
        combo.replace(vec!["One"]);
        assert_eq!(
            combo.reset_revision(),
            1,
            "even identical assignment resets host search"
        );
        combo.push("Two");
        assert_eq!(combo.reset_revision(), 1, "push preserves host search");
        assert_eq!(combo.options(), &["One", "Two"]);
        assert_eq!(
            Combo::restore(combo.options().to_vec(), combo.reset_revision()),
            combo
        );
    }
}
