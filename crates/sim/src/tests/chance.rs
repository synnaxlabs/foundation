//! Tests of the draw of a chance.

use crate::chance::under;

#[test]
fn a_chance_of_zero_holds_no_draw_and_a_chance_of_one_holds_every_draw() {
    assert!(!under(0, 0.0));
    assert!(under(u32::MAX, 1.0));
    assert!(under(u32::MAX / 2, 0.5) && !under(u32::MAX / 2 + 1, 0.5));
}
