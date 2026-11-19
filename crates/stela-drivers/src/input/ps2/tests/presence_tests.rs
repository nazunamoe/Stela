//! How a start decides a keyboard is there, as Linux's `atkbd_probe` and
//! `atkbd_skip_getid` decide it.
use super::*;

const TRANSLATING_FIRMWARE: u8 = TRANSLATION | 0x04 | IRQ_BITS;
/// SMBIOS chassis types: notebook, and desktop.
const NOTEBOOK: u8 = 10;
const DESKTOP: u8 = 3;

/// A start's result, with the controller and the model behind it.
type Started = (
    Result<(), Error>,
    Controller<Model, Time>,
    Rc<RefCell<Hardware>>,
);

fn start(chassis_type: Option<u8>, firmware: u8, change: impl FnOnce(&mut Hardware)) -> Started {
    let (mut c, h) = controller(Config {
        mouse: false,
        reset_on_start: false,
        query_scan_set: false,
        chassis_type,
        ..Config::default()
    });
    h.borrow_mut().config = firmware;
    change(&mut h.borrow_mut());
    let result = c.initialize();
    (result, c, h)
}

fn sent_getid(h: &Rc<RefCell<Hardware>>) -> bool {
    h.borrow().writes.contains(&(false, GET_ID))
}

#[test]
fn a_translated_keyboard_in_a_portable_chassis_is_kept_without_getid_even_when_mute() {
    // No key is pressed during the start, so no byte can take the
    // configuration byte's place.
    let (result, mut c, h) = start(Some(NOTEBOOK), TRANSLATING_FIRMWARE, |h| {
        h.keyboard_mute = true;
        h.scanning[0] = false;
    });
    result.unwrap();
    assert!(c.is_ready());
    assert!(!sent_getid(&h));
    assert_eq!(
        c.keyboard_presence(),
        Some(KeyboardPresence::Assumed {
            chassis_type: NOTEBOOK
        })
    );
    assert_eq!(
        describe_presence(c.keyboard_presence()),
        "keyboard taken as present without GETID, a translated port in portable chassis type 10"
    );
    // Set LEDs was tried and refused, so the rate was not sent, as atkbd
    // stops there.
    assert!(c.refused_commands().contains(&Command::Device {
        port: Port::Keyboard,
        byte: SET_LEDS
    }));
    assert!(!h.borrow().writes.contains(&(false, SET_RATE)));
    // Its keys still arrive, translated, and are delivered.
    emit(&h, Port::Keyboard, &[0x1c, 0xf0, 0x1c]);
    assert_eq!(
        c.poll().unwrap(),
        [
            Input::Key {
                usage: 4,
                pressed: true
            },
            Input::Key {
                usage: 4,
                pressed: false
            }
        ]
    );
    // A keyboard that answers is still set up, without GETID.
    let (result, c, h) = start(Some(NOTEBOOK), TRANSLATING_FIRMWARE, |_| {});
    result.unwrap();
    assert!(!sent_getid(&h));
    assert!(c.refused_commands().is_empty());
    assert_eq!(h.borrow().leds, 0);
}

#[test]
fn a_desktop_chassis_or_an_untranslated_port_keeps_getid_and_its_set_leds_check() {
    for (chassis_type, firmware) in [
        (Some(DESKTOP), TRANSLATING_FIRMWARE),
        (None, TRANSLATING_FIRMWARE),
        (Some(NOTEBOOK), IRQ_BITS),
    ] {
        let (result, c, h) = start(chassis_type, firmware, |_| {});
        result.unwrap();
        assert!(sent_getid(&h));
        assert_eq!(c.keyboard_presence(), Some(KeyboardPresence::Id));
        assert_eq!(
            describe_presence(c.keyboard_presence()),
            "keyboard found by its GETID answer"
        );
        // A keyboard that answers nothing is refused there, as Linux's
        // atkbd refuses it.
        let (result, c, _) = start(chassis_type, firmware, |h| {
            h.keyboard_mute = true;
            h.scanning[0] = false;
        });
        assert!(result.is_err());
        assert!(!c.is_ready());
    }
}

#[test]
fn getid_fails_to_set_leds_as_linux_and_a_device_that_is_no_keyboard_is_refused() {
    // One ID byte and no second, or no answer after the ACK: GETID failed
    // and Set LEDs proves the keyboard.
    for id in [vec![0xab], vec![]] {
        let (result, c, h) = start(Some(DESKTOP), IRQ_BITS, |h| h.keyboard_id = id);
        result.unwrap();
        assert_eq!(c.keyboard_presence(), Some(KeyboardPresence::Leds));
        assert_eq!(
            describe_presence(c.keyboard_presence()),
            "keyboard found by Set LEDs after GETID failed"
        );
        let data: Vec<u8> = h
            .borrow()
            .writes
            .iter()
            .filter_map(|&(command, byte)| (!command).then_some(byte))
            .collect();
        assert!(data.windows(2).any(|w| w == [SET_LEDS, 0]), "{data:02x?}");
    }
    // A mouse's ID, or a lone AA: not a keyboard.
    for id in [vec![0x00], vec![0xaa]] {
        let first = id[0];
        let (result, c, _) = start(Some(DESKTOP), IRQ_BITS, |h| h.keyboard_id = id);
        assert_eq!(
            result.unwrap_err().kind,
            ErrorKind::DeviceReply {
                port: Port::Keyboard,
                received: first
            }
        );
        assert_eq!(c.keyboard_presence(), None);
    }
}
