//! OS randomness on every supported desktop platform, with no fallback seed.

pub fn fill(bytes: &mut [u8]) -> std::io::Result<()> {
    getrandom::fill(bytes).map_err(|error| std::io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn operating_system_fills_buffers_and_accepts_empty_requests() {
        super::fill(&mut []).unwrap();
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        super::fill(&mut first).unwrap();
        super::fill(&mut second).unwrap();
        assert_ne!(first, [0u8; 32]);
        assert_ne!(first, second);
    }
}
