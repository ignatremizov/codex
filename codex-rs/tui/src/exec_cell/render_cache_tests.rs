use super::*;
use pretty_assertions::assert_eq;
use std::cell::Cell;

#[test]
fn settled_layout_reuses_rendering_and_skips_call_state_scan() {
    let cache = RenderCache::default();
    let renders = Cell::new(0);
    let render = || {
        renders.set(renders.get() + 1);
        vec![Line::from("complete output")]
    };
    let first = cache.render(
        RenderMode::Review,
        /*width*/ 80,
        /*theme_revision*/ 0,
        || false,
        render,
    );
    let second = cache.render(
        RenderMode::Review,
        /*width*/ 80,
        /*theme_revision*/ 0,
        || panic!("a cache hit must not walk calls"),
        || panic!("a cache hit must not render"),
    );
    assert_eq!(first, second);
    assert_eq!(renders.get(), 1);
}

#[test]
fn mode_width_and_theme_have_independent_cache_identity() {
    let cache = RenderCache::default();
    let renders = Cell::new(0);
    for (mode, width, theme_revision) in [
        (RenderMode::Review, 80, 0),
        (RenderMode::Full, 80, 0),
        (RenderMode::Review, 40, 0),
        (RenderMode::Review, 40, 1),
    ] {
        let lines = cache.render(
            mode,
            width,
            theme_revision,
            || false,
            || {
                renders.set(renders.get() + 1);
                vec![Line::from(format!("{width}/{theme_revision}"))]
            },
        );
        assert_eq!(lines, vec![Line::from(format!("{width}/{theme_revision}"))]);
    }
    assert_eq!(renders.get(), 4);
    assert_eq!(
        cache.render(
            RenderMode::Full,
            /*width*/ 80,
            /*theme_revision*/ 0,
            || panic!("Review layout changes must not evict Full"),
            || panic!("Full should retain its own layout"),
        ),
        vec![Line::from("80/0")]
    );
}

#[test]
fn live_rendering_never_populates_a_settled_entry() {
    let cache = RenderCache::default();
    for text in ["running", "more output", "finished"] {
        let lines = cache.render(
            RenderMode::Review,
            /*width*/ 80,
            /*theme_revision*/ 0,
            || text != "finished",
            || vec![Line::from(text)],
        );
        assert_eq!(lines, vec![Line::from(text)]);
    }
    assert_eq!(
        cache.render(
            RenderMode::Review,
            /*width*/ 80,
            /*theme_revision*/ 0,
            || false,
            || panic!("settled entry should be reused"),
        ),
        vec![Line::from("finished")]
    );
}
