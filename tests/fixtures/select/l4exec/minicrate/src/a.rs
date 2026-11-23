#[must_use]
pub fn fa() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_unit() {
        assert_eq!(super::fa(), 1);
    }
}
