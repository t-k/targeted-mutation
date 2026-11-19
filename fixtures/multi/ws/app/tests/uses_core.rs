//! A test in the dependent package that pins the boundary of `ws_core::is_large`.
#[test]
fn boundary_of_large() {
    assert!(!ws_core::is_large(1000));
    assert!(ws_core::is_large(1001));
}
