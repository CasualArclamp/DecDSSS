// made by CasualArclamp
//! The arithmetic of the tuning dial in the rig strip: what turning a digit
//! does to the frequency, and typing one in from a digit onwards. The drawing
//! and the mouse are in `rig_strip`; none of this touches `egui`, so it can be
//! tested alone.
//!
//! A frequency is nine decimal digits of hertz (`014.097.000`), so a digit is
//! named by its *place*: 0 is the hertz, 3 the kilohertz, 6 the megahertz and
//! 8 the hundreds of megahertz. The top is 999.999.999 Hz, which is above
//! anything the rigs here tune.

/// The digits of the dial.
pub const PLACES: usize = 9;

/// The highest frequency the dial can show, hertz.
pub const MAX_HZ: u64 = 999_999_999;

/// What one step of `place` is, in hertz.
pub fn place_value(place: usize) -> u64 {
    10u64.pow(place as u32)
}

/// The digit of `hz` at `place`.
pub fn digit(hz: u64, place: usize) -> u8 {
    (hz / place_value(place) % 10) as u8
}

/// `hz` turned `steps` clicks at `place` (negative is down): the carry goes up
/// into the next digit as on a real dial, and it stops at 0 and at the top
/// rather than wrapping.
pub fn step(hz: u64, place: usize, steps: i64) -> u64 {
    let moved = i128::from(hz) + i128::from(steps) * i128::from(place_value(place));
    moved.clamp(0, i128::from(MAX_HZ)) as u64
}

/// `hz` with the digit at `place` and every digit below it set to zero: what a
/// right click does, and where a typed entry starts from.
pub fn clear_from(hz: u64, place: usize) -> u64 {
    let unit = place_value(place + 1);
    hz / unit * unit
}

/// Whether the digit at `place` is a leading zero of `hz`, which the display
/// dims. The megahertz digit and those below it always count.
pub fn is_leading_zero(hz: u64, place: usize) -> bool {
    place > 6 && hz / place_value(place) == 0
}

/// A frequency being typed in from a digit onwards: the digits above the one
/// the pointer was over stay as they were, those typed replace the rest, one
/// place at a time, and those not yet typed are zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What has been typed, with the places not yet typed as zeros.
    pub hz: u64,
    /// Where the first digit went.
    first: usize,
    /// The place the next digit goes to; None once the last (the hertz) is in.
    next: Option<usize>,
}

impl Entry {
    /// Start at `place` of `hz`: everything from there down is cleared.
    pub fn begin(hz: u64, place: usize) -> Self {
        Self {
            hz: clear_from(hz, place),
            first: place,
            next: Some(place),
        }
    }

    /// The place the next digit will go to, for the cursor; None when full.
    pub fn cursor(&self) -> Option<usize> {
        self.next
    }

    /// Whether `place` has been typed into.
    pub fn typed(&self, place: usize) -> bool {
        place <= self.first && self.next.is_none_or(|next| place > next)
    }

    /// Type a digit; true when that was the last place, so the entry is done.
    pub fn push(&mut self, digit: u8) -> bool {
        let Some(place) = self.next else {
            return true;
        };
        debug_assert!(digit < 10);
        self.hz += u64::from(digit) * place_value(place);
        self.next = place.checked_sub(1);
        self.next.is_none()
    }

    /// Take the last digit back; false if there is none to take back.
    pub fn back(&mut self) -> bool {
        let place = match self.next {
            Some(next) if next < self.first => next + 1,
            None => 0,
            Some(_) => return false,
        };
        self.hz -= u64::from(digit(self.hz, place)) * place_value(place);
        self.next = Some(place);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digit_is_named_by_its_place() {
        assert_eq!(place_value(0), 1);
        assert_eq!(place_value(3), 1000);
        assert_eq!(place_value(6), 1_000_000);
        let hz = 14_097_250;
        let digits: Vec<u8> = (0..PLACES).rev().map(|p| digit(hz, p)).collect();
        assert_eq!(digits, [0, 1, 4, 0, 9, 7, 2, 5, 0]);
    }

    #[test]
    fn turning_a_digit_carries_and_stops_at_the_ends() {
        assert_eq!(step(14_097_000, 3, 1), 14_098_000);
        // 9 kHz up goes to 10 kHz: the carry is the next digit's.
        assert_eq!(step(14_099_000, 3, 1), 14_100_000);
        assert_eq!(step(14_100_000, 3, -1), 14_099_000);
        assert_eq!(step(14_097_000, 6, -3), 11_097_000);
        assert_eq!(step(500, 3, -1), 0);
        assert_eq!(step(0, 0, -5), 0);
        assert_eq!(step(MAX_HZ - 10, 1, 4), MAX_HZ);
        assert_eq!(step(14_097_000, 8, 1), 114_097_000);
    }

    #[test]
    fn clearing_from_a_digit_zeros_it_and_those_below() {
        assert_eq!(clear_from(14_097_250, 3), 14_090_000);
        assert_eq!(clear_from(14_097_250, 6), 10_000_000);
        assert_eq!(clear_from(14_097_250, 0), 14_097_250);
        assert_eq!(clear_from(14_097_250, 8), 0);
        assert_eq!(clear_from(0, 4), 0);
    }

    #[test]
    fn the_zeros_in_front_are_the_ones_dimmed() {
        // 7.074.000: the two hundreds-and-tens-of-megahertz digits are the
        // leading zeros, the megahertz digit is not.
        let lead: Vec<bool> = (0..PLACES)
            .rev()
            .map(|p| is_leading_zero(7_074_000, p))
            .collect();
        assert_eq!(
            lead,
            [true, true, false, false, false, false, false, false, false]
        );
        assert!(!is_leading_zero(0, 6));
        assert!(!is_leading_zero(145_500_000, 8));
    }

    #[test]
    fn typing_replaces_everything_from_the_digit_down() {
        // Over the megahertz digit of 14.097.250 the 1 above it stays, and the
        // rest is cleared: 7 0 7 4 types 17.074.000.
        let mut e = Entry::begin(14_097_250, 6);
        assert_eq!(e.hz, 10_000_000);
        assert_eq!(e.cursor(), Some(6));
        for (d, want) in [
            (7, 17_000_000),
            (0, 17_000_000),
            (7, 17_070_000),
            (4, 17_074_000),
        ] {
            assert!(!e.push(d));
            assert_eq!(e.hz, want, "after {d}");
        }
        // To type 7.074 the whole of it is typed from the tens of megahertz,
        // whose zero is the dimmed one.
        let mut e = Entry::begin(14_097_250, 7);
        assert_eq!(e.hz, 0);
        for d in [0, 7, 0, 7, 4] {
            assert!(!e.push(d));
        }
        assert_eq!(e.hz, 7_074_000);
    }

    #[test]
    fn typing_from_a_lower_digit_keeps_the_digits_above() {
        // Over the kilohertz units digit of 14.097.250: 14.09 stays, the 7 and
        // everything below it goes.
        let mut e = Entry::begin(14_097_250, 3);
        assert_eq!(e.hz, 14_090_000);
        e.push(1);
        e.push(2);
        e.push(3);
        assert_eq!(e.hz, 14_091_230);
        assert_eq!(e.cursor(), Some(0));
        assert!(e.push(5));
        assert_eq!(e.hz, 14_091_235);
        assert_eq!(e.cursor(), None);
    }

    #[test]
    fn the_typed_places_are_those_from_the_first_to_the_next() {
        let mut e = Entry::begin(14_097_250, 5);
        assert!(!e.typed(5));
        e.push(8);
        assert!(e.typed(5) && !e.typed(4) && !e.typed(6));
        e.push(1);
        assert!(e.typed(4) && !e.typed(3));
    }

    #[test]
    fn a_backspace_takes_the_last_digit_back_and_no_more() {
        let mut e = Entry::begin(14_097_250, 3);
        assert!(!e.back(), "nothing typed yet");
        e.push(4);
        e.push(5);
        assert_eq!(e.hz, 14_094_500);
        assert!(e.back());
        assert_eq!((e.hz, e.cursor()), (14_094_000, Some(2)));
        assert!(e.back());
        assert_eq!((e.hz, e.cursor()), (14_090_000, Some(3)));
        assert!(!e.back());
        // From the last place too.
        let mut e = Entry::begin(14_097_250, 0);
        assert!(e.push(9));
        assert_eq!(e.hz, 14_097_259);
        assert!(e.back());
        assert_eq!((e.hz, e.cursor()), (14_097_250, Some(0)));
    }
}
