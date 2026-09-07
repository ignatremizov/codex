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
    for surface in [WakeEventSurface::Agent, WakeEventSurface::UserShell] {
        assert!(
            WakeEventFlags::parse("", surface)
                .unwrap_err()
                .contains("empty")
        );
    }
}
