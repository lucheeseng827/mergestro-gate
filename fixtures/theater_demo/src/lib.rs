//! Known-positive fixture for the Slop Filter gate.
//!
//! `is_adult` has a boundary at 18. The test below *exercises* it but never
//! asserts the boundary — a classic "theater test". A mutation that flips
//! `>=` to `>` (so 18 is no longer an adult) survives, because no assertion
//! depends on the result. Phase 1's job is to surface exactly this.

/// Returns whether `age` clears the adulthood threshold (18).
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theater_test_exercises_without_asserting() {
        // Calls across the boundary, asserts nothing meaningful about it.
        // The suite is green, yet a boundary mutant goes uncaught.
        let _ = is_adult(20);
        let _ = is_adult(10);
        assert!(true);
    }
}
