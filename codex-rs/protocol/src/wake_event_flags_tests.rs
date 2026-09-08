use super::*;
use pretty_assertions::assert_eq;

#[test]
fn all_five_flag_sequences_obey_presence_and_pairwise_cancellation() {
    let alphabet = ['c', 'f', 'm', 'q', 'x'];
    // Exhaustive length-five inputs include every permutation, repeated presence flag,
    // and balanced/unbalanced f/x count representable in five characters.
    for mut code in 0..5_usize.pow(5) {
        let mut input = String::new();
        for _ in 0..5 {
            input.push(alphabet[code % 5]);
            code /= 5;
        }
        let wake = input.matches('f').count();
        let presentation = input.matches('x').count();
        let final_delivery = if wake > presentation {
            WakeEventFinalDelivery::Wake
        } else if presentation > wake {
            WakeEventFinalDelivery::PresentationOnly
        } else {
            WakeEventFinalDelivery::Passive
        };
        assert_eq!(
            WakeEventFlags::parse(&input, WakeEventSurface::Agent).unwrap(),
            WakeEventFlags {
                commentary: input.contains('c'),
                final_delivery,
                target_messages: input.contains('m'),
                queue_input: input.contains('q'),
                mailbox_input: false,
            },
            "{input}"
        );
    }
}

#[test]
fn shell_flags_share_cancellation_but_keep_their_smaller_alphabet() {
    for (input, canonical) in [
        ("qfx", "q"),
        ("qfxx", "qx"),
        ("xq", "qx"),
        ("xxffqq", "q"),
        ("xffqq", "fq"),
        ("qqq", "q"),
    ] {
        assert_eq!(
            WakeEventFlags::parse(input, WakeEventSurface::UserShell).unwrap(),
            WakeEventFlags::parse(canonical, WakeEventSurface::UserShell).unwrap()
        );
    }
    for (surface, input, invalid) in [
        (WakeEventSurface::Agent, "qz", 'z'),
        (WakeEventSurface::Agent, "fX", 'X'),
        (WakeEventSurface::UserShell, "xqc", 'c'),
        (WakeEventSurface::UserShell, "m", 'm'),
    ] {
        let error = WakeEventFlags::parse(input, surface).unwrap_err();
        assert!(
            error.contains(&format!("unknown flag `{invalid}`")),
            "{error}"
        );
    }
    for surface in [
        WakeEventSurface::Agent,
        WakeEventSurface::AgentMailbox,
        WakeEventSurface::UserShell,
    ] {
        assert!(
            WakeEventFlags::parse("", surface)
                .unwrap_err()
                .contains("empty")
        );
    }
}

#[test]
fn mailbox_flags_normalize_order_repetition_and_cancelled_wakes() {
    let expected = WakeEventFlags {
        commentary: false,
        final_delivery: WakeEventFinalDelivery::Passive,
        target_messages: false,
        queue_input: false,
        mailbox_input: true,
    };
    for input in [
        "z", "zz", "zx", "xz", "zfx", "fxz", "xzf", "zfxx", "xxfz", "zzxxff", "zxxx",
    ] {
        assert_eq!(
            WakeEventFlags::parse(input, WakeEventSurface::AgentMailbox),
            Ok(expected),
            "{input}"
        );
    }
}

#[test]
fn mailbox_surface_preserves_agent_flags_and_checks_normalized_compatibility() {
    let alphabet = ['c', 'f', 'm', 'q', 'x', 'z'];
    // Exhaustive length-five inputs cover every ordering of mailbox flags with
    // presence flags and balanced or unbalanced final-delivery flags.
    for mut code in 0..6_usize.pow(5) {
        let mut input = String::new();
        for _ in 0..5 {
            input.push(alphabet[code % 6]);
            code /= 6;
        }
        let actual = WakeEventFlags::parse(&input, WakeEventSurface::AgentMailbox);
        if !input.contains('z') {
            assert_eq!(
                actual,
                WakeEventFlags::parse(&input, WakeEventSurface::Agent),
                "{input}"
            );
        } else if input.contains(['c', 'm', 'q'])
            || input.matches('f').count() > input.matches('x').count()
        {
            assert_eq!(
                actual,
                Err(
                    "mailbox flag `z` cannot be combined with c, m, q, or effective wake delivery"
                        .to_string()
                ),
                "{input}"
            );
        } else {
            assert_eq!(
                actual,
                Ok(WakeEventFlags {
                    commentary: false,
                    final_delivery: WakeEventFinalDelivery::Passive,
                    target_messages: false,
                    queue_input: false,
                    mailbox_input: true,
                }),
                "{input}"
            );
        }
    }
}

#[test]
fn mailbox_incompatible_flags_are_rejected_even_after_cancellation() {
    for input in [
        "zf", "fz", "zffx", "xffz", "zc", "mz", "zq", "zfxc", "mxzf", "qzfx",
    ] {
        assert_eq!(
            WakeEventFlags::parse(input, WakeEventSurface::AgentMailbox),
            Err(
                "mailbox flag `z` cannot be combined with c, m, q, or effective wake delivery"
                    .to_string()
            ),
            "{input}"
        );
    }
}

#[test]
fn mailbox_syntax_requires_explicit_surface_opt_in() {
    for (surface, alphabet) in [
        (WakeEventSurface::Agent, "c, f, m, q, or x"),
        (WakeEventSurface::UserShell, "f, q, or x"),
    ] {
        for input in ["z", "zx", "zfx", "fxz"] {
            assert_eq!(
                WakeEventFlags::parse(input, surface),
                Err(format!("unknown flag `z`; use {alphabet}")),
                "{input}"
            );
        }
    }
    for (input, flag) in [("zX", 'X'), ("z?", '?'), ("z ", ' '), ("zé", 'é')] {
        assert_eq!(
            WakeEventFlags::parse(input, WakeEventSurface::AgentMailbox),
            Err(format!("unknown flag `{flag}`; use c, f, m, q, x, or z")),
            "{input}"
        );
    }
    assert_eq!(
        WakeEventFlags::parse("", WakeEventSurface::AgentMailbox),
        Err("empty flags; omit w for default handling or use c, f, m, q, x, or z".to_string())
    );
}
