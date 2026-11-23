#[must_use]
pub fn solo() -> u32 {
    7
}

#[cfg(test)]
mod tests {
    #[test]
    fn solo_unit() {
        assert_eq!(super::solo(), 7);
    }
}
