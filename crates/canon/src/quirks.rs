//! Per-model differences from gphoto's `is_canon_eos_m` and `camera_prepare_canon_eos_capture`.

const OP_REMOTE_RELEASE_ON: u16 = 0x9128;

/// The EOS M line, and the newer `PowerShot` G, SX and IXUS bodies that run its firmware.
pub(crate) fn eos_m(model: &str, ops: &[u16]) -> bool {
    if model.starts_with("Canon EOS M") {
        return true;
    }
    [
        "Canon PowerShot SX",
        "Canon PowerShot G",
        "Canon Digital IXUS",
    ]
    .iter()
    .any(|p| model.starts_with(p))
        && ops.contains(&OP_REMOTE_RELEASE_ON)
}

/// The `SetRemoteMode` value that starts a session, or `None` for a body that must not get one.
pub(crate) fn remote_mode(model: &str, ops: &[u16]) -> Option<u32> {
    if !eos_m(model, ops) {
        return Some(1);
    }
    match model {
        "Canon PowerShot SX600 HS" => None,
        "Canon PowerShot SX540 HS" | "Canon PowerShot G5 X" => Some(0x11),
        "Canon EOS M6 Mark II" => Some(1),
        _ => Some(0x15),
    }
}

/// Event mode 2 and live view output 8 before streaming. The M6 Mark II and M50 Mark II act
/// like an SLR and turn tethering off if sent them.
pub(crate) fn wants_output_8(model: &str, ops: &[u16]) -> bool {
    eos_m(model, ops) && !matches!(model, "Canon EOS M6 Mark II" | "Canon EOS M50m2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slrs_and_r_bodies_get_the_plain_session() {
        for model in ["Canon EOS REBEL T3", "Canon EOS R6", "Canon EOS 5D Mark IV"] {
            assert_eq!(
                remote_mode(model, &[OP_REMOTE_RELEASE_ON]),
                Some(1),
                "{model}"
            );
            assert!(!wants_output_8(model, &[OP_REMOTE_RELEASE_ON]), "{model}");
        }
    }

    #[test]
    fn eos_m_and_its_powershot_cousins() {
        assert_eq!(remote_mode("Canon EOS M50", &[]), Some(0x15));
        assert!(wants_output_8("Canon EOS M50", &[]));
        assert_eq!(remote_mode("Canon EOS M6 Mark II", &[]), Some(1));
        assert!(!wants_output_8("Canon EOS M6 Mark II", &[]));
        assert!(!wants_output_8("Canon EOS M50m2", &[]));
        assert_eq!(
            remote_mode("Canon PowerShot G5 X", &[OP_REMOTE_RELEASE_ON]),
            Some(0x11)
        );
        assert_eq!(
            remote_mode("Canon PowerShot SX600 HS", &[OP_REMOTE_RELEASE_ON]),
            None
        );
        assert_eq!(
            remote_mode("Canon PowerShot G7 X", &[OP_REMOTE_RELEASE_ON]),
            Some(0x15)
        );
        // A PowerShot without the EOS release is an old PowerShot.
        assert_eq!(remote_mode("Canon PowerShot G7 X", &[]), Some(1));
    }
}
