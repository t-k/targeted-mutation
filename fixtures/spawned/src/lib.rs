//! Code that is only run by the program a test starts. A mutant of it that cannot be
//! run as a probe stops that program, and the test sees a program that failed.

/// Counts through an empty range twice. With the end of the range taken in, the first
/// count is 1 and the second range ends at the largest value there is.
pub fn rounds() -> usize {
    let mut end = 0u8;
    let mut n = 0;
    let mut round = 0;
    while round != 2 {
        n = 0;
        for _ in end..end {
            n += 1;
        }
        if n > 0 {
            end = u8::MAX;
        }
        round += 1;
    }
    n
}

/// Checked by a test that ends the whole test program when the answer is wrong, with a
/// status of its own choosing.
pub fn double(x: u32) -> u32 {
    x * 2
}
