use super::HeadTailBuffer;
use super::MAX_UPSTREAM_OMISSION_BOUNDARIES;
use super::MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION;

use pretty_assertions::assert_eq;

#[test]
fn keeps_prefix_and_suffix_when_over_budget() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    buf.push_chunk(b"0123456789".to_vec());
    assert_eq!(buf.omitted_bytes(), 0);

    // Exceeds max by 2; we should keep head+tail and omit the middle.
    buf.push_chunk(b"ab".to_vec());
    assert!(buf.omitted_bytes() > 0);

    let rendered = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(rendered.starts_with("01234"));
    assert!(rendered.ends_with("89ab"));
    assert_eq!(
        String::from_utf8_lossy(&buf.to_bytes_with_omission_marker()),
        "01234\n... 2 bytes omitted ...\n789ab"
    );
}

#[test]
fn max_bytes_zero_drops_everything() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 0);
    buf.push_chunk(b"abc".to_vec());

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 3);
    assert_eq!(buf.to_bytes(), b"".to_vec());
    assert_eq!(buf.snapshot_chunks(), Vec::<Vec<u8>>::new());
}

#[test]
fn head_budget_zero_keeps_only_last_byte_in_tail() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 1);
    buf.push_chunk(b"abc".to_vec());

    assert_eq!(buf.retained_bytes(), 1);
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.to_bytes(), b"c".to_vec());
}

#[test]
fn draining_resets_state_and_push_buffer_preserves_omissions() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"0123456789".to_vec());
    buf.push_chunk(b"ab".to_vec());

    let drained = buf.drain();
    let mut collected = HeadTailBuffer::new(/*max_bytes*/ 10);
    collected.push_buffer(drained);

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 0);
    assert_eq!(buf.to_bytes(), b"".to_vec());
    assert_eq!(collected.to_bytes(), b"01234789ab".to_vec());
    assert_eq!(collected.omitted_bytes(), 2);
    assert_eq!(collected.total_bytes(), 12);
}

#[test]
fn upstream_omission_stays_at_append_boundary() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 128);
    buf.push_chunk(b"before".to_vec());
    buf.push_upstream_omission(/*omitted_bytes*/ 6);
    buf.push_chunk(b"after".to_vec());

    assert_eq!(
        (
            buf.to_bytes_with_omission_marker(),
            buf.omitted_bytes(),
            buf.total_bytes(),
        ),
        (b"before\n... 6 bytes omitted ...\nafter".to_vec(), 6, 17,)
    );
}

#[test]
fn truncated_upstream_boundary_does_not_count_marker_text_as_output() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"abcdefgh".to_vec());
    buf.push_upstream_omission(/*omitted_bytes*/ 6);
    buf.push_chunk(b"0123456789".to_vec());

    assert_eq!(
        (
            buf.to_bytes_with_omission_marker(),
            buf.omitted_bytes(),
            buf.total_bytes(),
        ),
        (b"abcde\n... 14 bytes omitted ...\n56789".to_vec(), 14, 24)
    );
    assert_eq!(buf.middle_upstream_omitted_bytes, 6);
    assert!(buf.upstream_omissions.is_empty());
}

#[test]
fn upstream_omissions_in_truncated_middle_are_aggregated() {
    let omission_count = MAX_UPSTREAM_OMISSION_BOUNDARIES + 100;
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    for _ in 0..omission_count {
        buf.push_chunk(b"x".to_vec());
        buf.push_upstream_omission(/*omitted_bytes*/ 1);
    }

    assert_eq!(
        (buf.retained_bytes(), buf.omitted_bytes(), buf.total_bytes()),
        (10, omission_count * 2 - 10, omission_count * 2)
    );
    assert!(buf.middle_upstream_omitted_bytes > 0);
    assert!(buf.upstream_omissions.len() <= MAX_UPSTREAM_OMISSION_BOUNDARIES);
}

#[test]
fn upstream_omissions_at_same_boundary_are_coalesced() {
    let omission_count = MAX_UPSTREAM_OMISSION_BOUNDARIES + 100;
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"x".to_vec());
    for _ in 0..omission_count {
        buf.push_upstream_omission(/*omitted_bytes*/ 1);
    }

    assert_eq!(
        (
            buf.omitted_bytes(),
            buf.upstream_omissions.len(),
            buf.to_bytes_with_omission_marker(),
        ),
        (
            omission_count,
            1,
            format!("x\n... {omission_count} bytes omitted ...\n").into_bytes(),
        )
    );
}

#[test]
fn retained_upstream_omission_boundaries_are_bounded() {
    let omission_count = MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION + 100;
    let unpositioned_omission_count = omission_count - MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION;
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ omission_count * 2);
    for _ in 0..omission_count {
        buf.push_chunk(b"x".to_vec());
        buf.push_upstream_omission(/*omitted_bytes*/ 1);
    }

    assert_eq!(
        (
            buf.retained_bytes(),
            buf.omitted_bytes(),
            buf.total_bytes(),
            buf.upstream_omissions.len(),
        ),
        (
            omission_count,
            omission_count,
            omission_count * 2,
            MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION,
        )
    );
    let rendered = buf.to_bytes_with_omission_marker();
    let unpositioned_notice = format!(
        "Warning: {unpositioned_omission_count} bytes were omitted at multiple locations in retained process output.\n"
    );
    assert_eq!(
        (
            buf.unpositioned_upstream_omitted_bytes,
            buf.upstream_omissions
                .first()
                .map(|omission| omission.output_bytes_before),
            String::from_utf8_lossy(&rendered).starts_with(unpositioned_notice.as_str()),
            rendered
                .windows(b"bytes omitted".len())
                .filter(|window| *window == b"bytes omitted")
                .count(),
        ),
        (
            unpositioned_omission_count,
            Some(unpositioned_omission_count + 1),
            true,
            MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION,
        )
    );
}

#[test]
fn chunk_larger_than_tail_budget_keeps_only_tail_end() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"0123456789".to_vec());

    // Tail budget is 5 bytes. This chunk should replace the tail and keep only its last 5 bytes.
    buf.push_chunk(b"ABCDEFGHIJK".to_vec());

    let out = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(out.starts_with("01234"));
    assert!(out.ends_with("GHIJK"));
    assert!(buf.omitted_bytes() > 0);
}

#[test]
fn fills_head_then_tail_across_multiple_chunks() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    // Fill the 5-byte head budget across multiple chunks.
    buf.push_chunk(b"01".to_vec());
    buf.push_chunk(b"234".to_vec());
    assert_eq!(buf.to_bytes(), b"01234".to_vec());

    // Then fill the 5-byte tail budget.
    buf.push_chunk(b"567".to_vec());
    buf.push_chunk(b"89".to_vec());
    assert_eq!(buf.to_bytes(), b"0123456789".to_vec());
    assert_eq!(buf.omitted_bytes(), 0);

    // One more byte causes the tail to drop its oldest byte.
    buf.push_chunk(b"a".to_vec());
    assert_eq!(buf.to_bytes(), b"012346789a".to_vec());
    assert_eq!(buf.omitted_bytes(), 1);
}

#[test]
fn empty_and_tiny_chunks_have_bounded_metadata() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    for byte in b"0123456789ab" {
        buf.push_chunk(Vec::new());
        buf.push_chunk(vec![*byte]);
    }

    assert_eq!(
        buf.snapshot_chunks(),
        vec![b"01234".to_vec(), b"789ab".to_vec()]
    );
    assert_eq!(buf.retained_bytes(), 10);
    assert_eq!(buf.omitted_bytes(), 2);
}
