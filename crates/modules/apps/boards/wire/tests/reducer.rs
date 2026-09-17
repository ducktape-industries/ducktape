//! The shape of the reducer, guarded by parsing it.
//!
//! A board's `apply` is the consensus fold: one exhaustive dispatch that does
//! nothing but delegate, so a new event fails the build until it is routed and
//! no arm can quietly decide something on the way. The shape is load-bearing
//! and invisible to a behaviour test, so it is parsed rather than described.

#[test]
fn the_consensus_reducer_has_one_exhaustive_delegating_dispatch() {
    let file = syn::parse_file(include_str!("../src/lib.rs")).unwrap();
    let reducer = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(item) => Some(item),
            _ => None,
        })
        .flat_map(|item| &item.items)
        .find_map(|item| match item {
            syn::ImplItem::Fn(method) if method.sig.ident == "apply" => Some(method),
            _ => None,
        })
        .unwrap();
    let [syn::Stmt::Expr(syn::Expr::Match(dispatch), None)] = reducer.block.stmts.as_slice() else {
        panic!("one dispatch only")
    };
    for arm in &dispatch.arms {
        assert!(arm.guard.is_none());
        assert!(!matches!(arm.pat, syn::Pat::Wild(_)));
        assert!(
            matches!(*arm.body, syn::Expr::MethodCall(_)),
            "each event delegates to a named pure handler"
        );
    }
}
