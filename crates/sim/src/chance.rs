//! Draws of a chance, from 0 to 1, from a seeded stream.

use env::rng::Rng;

/// Whether `chance` is from 0 to 1.
pub(crate) fn valid(chance: f64) -> bool {
    (0.0..=1.0).contains(&chance)
}

/// Draws from `rng`, and gives whether the draw falls under `chance`.
pub(crate) fn roll(rng: &mut Rng, chance: f64) -> bool {
    let draw = u32::try_from(rng.next_u64() >> 32)
        .expect("invariant: the high half of a u64 fits u32");
    under(draw, chance)
}

/// Whether `draw`, uniform over `u32`, falls under `chance`. A chance of 0 never
/// holds a draw, and a chance of 1 holds every draw.
pub(crate) fn under(draw: u32, chance: f64) -> bool {
    f64::from(draw) < chance * 2f64.powi(32)
}
