//! Depends on a sibling member and on a crate outside the workspace.

pub fn label(size: u64) -> &'static str {
    if ws_core::is_large(size) && outside::enabled() {
        "large"
    } else if size == 0 {
        "empty"
    } else {
        "small"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn labels() {
        assert_eq!(super::label(5000), "large");
        assert_eq!(super::label(0), "empty");
        assert_eq!(super::label(3), "small");
    }
}
