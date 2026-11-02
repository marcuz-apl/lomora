/// Python's round() is round-half-to-even; f64::round is half-away-from-zero and the two
/// disagree on exact ties. Rust's float formatting is correctly rounded, so format-then-
/// parse reproduces CPython. Spec 5.1.
pub fn round_half_even(x: f64, digits: u32) -> f64 {
    match format!("{:.*}", digits as usize, x).parse::<f64>() {
        Ok(v) => v,
        Err(_) => x,
    }
}

#[cfg(test)]
mod tests {
    use super::round_half_even;

    // Expected values are CPython's answers, measured against the reference interpreter rather
    // than reasoned out: round(0.125, 2)=0.12, round(0.135, 2)=0.14, round(1.5)=2, round(2.5)=2,
    // round(0.5)=0, round(3.5)=4, round(1.25, 1)=1.2, round(10155.5)=10156. The implementation
    // reproduces all eight. Note 1.5 goes UP to 2 — the even neighbour of a tie is not always
    // the lower one, and `half-away-from-zero` would give the same 2, so 2.5 and 0.5 are the
    // assertions that actually separate the two rules.
    #[test]
    fn rounds_half_to_even_not_away() {
        assert_eq!(round_half_even(0.125, 2), 0.12);
        assert_eq!(round_half_even(0.135, 2), 0.14);
        assert_eq!(round_half_even(1.5, 0), 2.0);
        assert_eq!(round_half_even(2.5, 0), 2.0);
        assert_eq!(round_half_even(0.5, 0), 0.0);
        assert_eq!(round_half_even(3.5, 0), 4.0);
        assert_eq!(round_half_even(1.25, 1), 1.2);
    }

    #[test]
    fn rounds_mebibyte_ties_like_pynvml() {
        assert_eq!(round_half_even(10155.5, 0), 10156.0);
    }

    #[test]
    fn leaves_clean_values_alone() {
        assert_eq!(round_half_even(23.5, 1), 23.5);
        assert_eq!(round_half_even(1200.0, 1), 1200.0);
    }
}
