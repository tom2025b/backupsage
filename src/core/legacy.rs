//! Rows written by older indexers (#105), and what their names can prove.
//!
//! The index loader marks two kinds of legacy row with reader-only flags,
//! which are never written to any index:
//!
//! - [`flags::LOSSY_PATH`]: an index from before v1.0.1 stored a non-UTF-8
//!   name only as its lossy rendering, so the name holds U+FFFD and the real
//!   bytes are unknown.
//! - [`flags::LEGACY_SPARSE`]: an indexer from before #63 read a PAX-sparse
//!   member through tar-rs. It stored the condensed stream's hash and size,
//!   and for 0.1 and 1.0 the synthetic `%d/GNUSparseFile.%p/%f` name.
//!
//! Such a row's stored name is not its key in the effective namespace. It
//! stands for a small set of names it could really be, and no rule may
//! resolve that set to one of them. `diff` and coverage use this module to
//! ask which exact paths a legacy row could be, and so could shadow or be
//! mistaken for.

use crate::store::flags;

/// Whether `row_flags` mark a name that is not the row's exact path.
pub fn name_uncertain(row_flags: i64) -> bool {
    row_flags & (flags::LOSSY_PATH | flags::LEGACY_SPARSE) != 0
}

/// The names an uncertain row could really have, as stored bytes: the stored
/// name itself and, for a legacy sparse row whose stored name is GNU tar's
/// `%d/GNUSparseFile.%p/%f` wrapper, `%d/%f` (and `%f` when `%d` is `.`).
/// Empty for a row whose name is exact.
pub fn candidates(path: &[u8], row_flags: i64) -> Vec<Vec<u8>> {
    if !name_uncertain(row_flags) {
        return Vec::new();
    }
    let mut out = vec![path.to_vec()];
    if row_flags & flags::LEGACY_SPARSE != 0 {
        out.extend(unwrap_sparse(path));
    }
    out
}

/// Whether an exact `path` could be the real name behind `candidate`. A
/// lossy candidate matches every path that renders the same.
pub fn could_be(candidate: &[u8], path: &[u8]) -> bool {
    candidate == path || candidate == String::from_utf8_lossy(path).as_bytes()
}

/// The real names GNU tar's sparse wrapper `%d/GNUSparseFile.%p/%f` stands
/// for: `%d/%f`, plus `%f` when `%d` is `.` (GNU tar writes `./` for a
/// member at the top level).
fn unwrap_sparse(path: &[u8]) -> Vec<Vec<u8>> {
    const MARK: &[u8] = b"GNUSparseFile.";
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(at) = find(&path[start..], MARK).map(|i| start + i) {
        start = at + 1;
        // The wrapper is a whole component: at the start or after '/'.
        if at > 0 && path[at - 1] != b'/' {
            continue;
        }
        let digits = &path[at + MARK.len()..];
        let n = digits.iter().take_while(|b| b.is_ascii_digit()).count();
        if n == 0 || digits.get(n) != Some(&b'/') || digits.len() == n + 1 {
            continue;
        }
        let (dir, file) = (&path[..at], &digits[n + 1..]);
        out.push([dir, file].concat());
        if dir == b"./" {
            out.push(file.to_vec());
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_names_have_no_candidates() {
        assert!(candidates(b"a/GNUSparseFile.1/b", flags::SPARSE).is_empty());
    }

    #[test]
    fn a_sparse_wrapper_stands_for_its_real_name() {
        let c = candidates(b"./GNUSparseFile.1370099/holey.bin", flags::LEGACY_SPARSE);
        assert_eq!(
            c,
            vec![
                b"./GNUSparseFile.1370099/holey.bin".to_vec(),
                b"./holey.bin".to_vec(),
                b"holey.bin".to_vec(),
            ]
        );
        let c = candidates(b"d/e/GNUSparseFile.7/f.img", flags::LEGACY_SPARSE);
        assert_eq!(c[1], b"d/e/f.img");
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn only_a_whole_wrapper_component_unwraps() {
        for path in [
            &b"xGNUSparseFile.1/f"[..],
            b"GNUSparseFile./f",
            b"GNUSparseFile.12x/f",
            b"GNUSparseFile.12/",
        ] {
            assert_eq!(candidates(path, flags::LEGACY_SPARSE).len(), 1, "{path:?}");
        }
    }

    #[test]
    fn a_lossy_candidate_matches_every_rendering() {
        assert!(could_be("d\u{fffd}".as_bytes(), b"d\xff"));
        assert!(could_be("d\u{fffd}".as_bytes(), b"d\xfe"));
        assert!(!could_be(b"d", b"d\xff"));
        assert!(could_be(b"holey.bin", b"holey.bin"));
    }
}
