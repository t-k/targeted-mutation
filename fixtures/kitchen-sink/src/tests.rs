use super::*;

#[test]
fn clamps() {
    assert_eq!(clamp_percent(-5), 0);
    assert_eq!(clamp_percent(50), 50);
    assert_eq!(clamp_percent(150), 100);
    assert!(const_limit(3));
}

#[test]
fn generic_max() {
    assert_eq!(max_of(1, 2), 2);
    assert_eq!(max_of(2, 2), 2);
    assert_eq!(max_of(3, 2), 3);
    assert_eq!(max_of("a", "b"), "b");
}

#[test]
fn hot_and_cold() {
    assert!(is_hot(40.0));
    assert!(!is_hot(10.0));
}

#[test]
fn text_and_slices() {
    assert!(same_text("a", &"a".to_string()));
    assert!(!same_text("a", &"b".to_string()));
    assert!(non_empty(&[1]));
    assert!(!non_empty(&[]));
    assert!(unsigned_floor(0));
    assert!(unsigned_floor(3));
    assert_eq!(count_small(&[1, 2, 3], 3), 2);
    assert!(ref_less(&1, &2));
    assert!(!ref_less(&2, &2));
    assert!(!ref_less(&3, &2));
}

#[test]
fn sums() {
    assert_eq!(sum_until(&[1, 2, 9, 4], 9), 3);
    assert_eq!(sum_until(&[1, 2], 9), 3);
    assert_eq!(late_init(true), 1);
    assert_eq!(late_init(false), 2);
}

#[test]
fn counts_hits_per_key() {
    let mut counter = Counter::default();
    counter.hit("a");
    counter.hit("a");
    assert_eq!(counter.get("a"), 2);
    assert_eq!(counter.get("b"), 0);
}

#[test]
fn threads() {
    assert!(in_thread(9));
    assert!(!in_thread(1));
}

#[test]
#[should_panic(expected = "gave up")]
fn gives_up() {
    give_up();
}
