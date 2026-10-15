//! `OverlayFs` merges two filesystems that each number their own revisions,
//! so a revision it reported could name two different contents. It reports
//! none: a caller that builds a precondition on it must not be handed a
//! number that is not unique to one content.

use simulacra_types::VirtualFs;
use simulacra_vfs::{MemoryFs, OverlayFs};

fn overlay_over(lower: MemoryFs) -> OverlayFs {
    OverlayFs::new(Box::new(lower), Box::new(MemoryFs::new()))
}

#[test]
fn positive_control_the_layers_do_report_revisions_on_their_own() {
    let lower = MemoryFs::new();
    lower.write("/a", b"old").unwrap();
    assert!(
        lower.metadata("/a").unwrap().revision.is_some(),
        "the premise: MemoryFs revisions exist, so the overlay's choice is observable"
    );
}

#[test]
fn a_file_read_through_from_the_lower_layer_reports_no_revision() {
    let lower = MemoryFs::new();
    lower.write("/a", b"old").unwrap();
    let overlay = overlay_over(lower);
    assert_eq!(overlay.metadata("/a").unwrap().revision, None);
}

#[test]
fn a_file_written_through_the_overlay_reports_no_revision() {
    let lower = MemoryFs::new();
    lower.write("/a", b"old").unwrap();
    let overlay = overlay_over(lower);
    overlay.write("/a", b"new").unwrap();
    assert_eq!(overlay.read("/a").unwrap(), b"new");
    assert_eq!(overlay.metadata("/a").unwrap().revision, None);
}

#[test]
fn the_same_revision_never_names_two_contents() {
    let lower = MemoryFs::new();
    lower.write("/a", b"old").unwrap();
    let overlay = overlay_over(lower);
    let before = overlay.metadata("/a").unwrap().revision;
    let before_bytes = overlay.read("/a").unwrap();
    overlay.write("/a", b"new").unwrap();
    let after = overlay.metadata("/a").unwrap().revision;
    let after_bytes = overlay.read("/a").unwrap();
    assert_ne!(
        before_bytes, after_bytes,
        "positive control: contents differ"
    );
    assert!(
        before.is_none() || before != after,
        "revision {before:?} named both {before_bytes:?} and {after_bytes:?}"
    );
}
