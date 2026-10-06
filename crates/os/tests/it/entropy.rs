#[test]
fn two_fills_differ() {
    let entropy = os::entropy();
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    entropy.fill(&mut a);
    entropy.fill(&mut b);
    assert_ne!(a, b);
}

#[test]
fn a_large_fill_has_no_zero_chunk() {
    let entropy = os::entropy();
    let mut bytes = vec![0u8; 64 * 1024];
    entropy.fill(&mut bytes);
    let zero = bytes
        .chunks(256)
        .position(|chunk| chunk.iter().all(|&b| b == 0));
    assert_eq!(zero, None, "an all-zero chunk");
}

#[test]
fn an_empty_fill_returns() {
    os::entropy().fill(&mut []);
}
