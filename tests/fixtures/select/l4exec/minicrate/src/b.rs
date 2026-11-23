#[must_use]
pub fn fb() -> u32 {
    2
}

#[cfg(test)]
mod tests {
    #[test]
    fn b_unit() {
        assert_eq!(super::fb(), 2);
    }

    #[test]
    #[ignore = "ignoré par construction : nextest ne le lance pas"]
    fn b_ignored() {
        assert_eq!(super::fb(), 2);
    }
}
