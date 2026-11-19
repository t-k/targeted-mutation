use kitchen_sink::parse_positive;

#[test]
fn parses_numbers() {
    assert_eq!(parse_positive("3"), Ok(3));
    assert!(parse_positive("x").is_err());
}
