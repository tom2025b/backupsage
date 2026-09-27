//! Rows whose names an older indexer did not record exactly (#105).
//!
//! An index from before v1.0.1 stored a non-UTF-8 name only as its lossy
//! rendering, so the name holds U+FFFD and the real bytes are unknown. The
//! index loader marks such a row with [`flags::LOSSY_PATH`], a reader-only
//! flag that is never written to any index. Its stored name is not its key
//! in the effective namespace: it stands for every name that renders the
//! same, and no rule may resolve it to one of them. `diff` and coverage use
//! this module to ask which exact paths such a row could be.
//!
//! (An index from before #63 that holds sparse rows is refused outright:
//! their real names came from `GNU.sparse.name`, which the old indexer
//! never recorded and which can be anything, so no finite rule could say
//! what they stand for.)

use crate::store::flags;

/// Whether `row_flags` mark a name that is not the row's exact path.
pub fn name_uncertain(row_flags: i64) -> bool {
    row_flags & flags::LOSSY_PATH != 0
}

/// Whether an exact `path` could be the real name behind the lossy stored
/// name `stored`: whether `path` renders as `stored`.
pub fn could_be(stored: &[u8], path: &[u8]) -> bool {
    stored == String::from_utf8_lossy(path).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lossy_name_stands_for_every_path_that_renders_the_same() {
        assert!(could_be("d\u{fffd}".as_bytes(), b"d\xff"));
        assert!(could_be("d\u{fffd}".as_bytes(), b"d\xfe"));
        assert!(!could_be("d\u{fffd}".as_bytes(), b"d"));
        assert!(!could_be("d\u{fffd}".as_bytes(), b"e\xff"));
    }

    #[test]
    fn only_the_lossy_flag_makes_a_name_uncertain() {
        assert!(name_uncertain(flags::LOSSY_PATH));
        assert!(!name_uncertain(flags::LOSSY_LINK_TARGET | flags::SPARSE));
    }
}
