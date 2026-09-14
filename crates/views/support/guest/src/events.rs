//! Live opt-in for host window and input-method observations.
/// Invoke inside the active subscription branch, including cached recipes.
/// This does not request permission for any host task effect.
pub fn observe<Message>(
    subscription: crate::Subscription<Message>,
    interest: crate::wire::events::Interest,
) -> crate::Subscription<Message> {
    crate::slots::include_event_interest(interest);
    subscription
}
