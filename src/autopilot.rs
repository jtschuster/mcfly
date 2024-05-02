use winapi::um::winuser::KEYBDINPUT;

pub fn type_string(string: &str) {
    for c in string.chars() {
        tap(&Character(c));
    }
}

trait KeyCodeConvertible {
    #[cfg(target_os = "macos")]
    fn code(&self) -> CGKeyCode;
    #[cfg(target_os = "linux")]
    fn code(&self) -> XKeyCode;
    #[cfg(windows)]
    fn code(&self) -> WinKeyCode;
    fn character(&self) -> Option<char> {
        None
    }
}

#[cfg(windows)]
type WinKeyCode = i32;

#[derive(Copy, Clone, Debug)]
struct Character(pub char);

impl KeyCodeConvertible for Character {
    fn character(&self) -> Option<char> {
        Some(self.0)
    }

    #[cfg(target_os = "macos")]
    fn code(&self) -> CGKeyCode {
        char_to_key_code(self.0)
    }

    #[cfg(windows)]
    fn code(&self) -> WinKeyCode {
        panic!("Unsupported OS")
    }

    #[cfg(target_os = "linux")]
    fn code(&self) -> XKeyCode {
        char_to_key_code(self.0)
    }
}

fn tap<T: KeyCodeConvertible + Copy>(key: &T) {
    toggle(key, true);
    toggle(key, false);
}

/// Holds down the given key or keycode if `down` is `true`, or releases it if
/// not. Characters are converted to a keycode corresponding to the current
/// keyboard layout. Delay between pressing and releasing the modifier keys can
/// be controlled using the `modifier_delay_ms` parameter.
fn toggle<T: KeyCodeConvertible>(key: &T, down: bool) {
    let _key_flags = key.character();
    system_toggle(key, down);
}

#[cfg(windows)]
fn system_toggle<T: KeyCodeConvertible>(key: &T, down: bool) {
    use std::mem::size_of;
    use winapi::um::winuser::{
        INPUT_u, SendInput, INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    };

    if let Some(character) = key.character() {
        let flags = if down { 0 } else { KEYEVENTF_KEYUP };
        let mut buf = [0; 2];
        for word in character.encode_utf16(&mut buf) {
            let mut input = INPUT {
                type_: INPUT_KEYBOARD,
                u: unsafe {
                    std::mem::transmute_copy(&(
                        KEYBDINPUT {
                            wVk: 0,
                            wScan: *word,
                            dwFlags: KEYEVENTF_UNICODE | flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                        [0; size_of::<INPUT_u>() - size_of::<KEYBDINPUT>()],
                    ))
                },
            };
            unsafe {
                SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
            }
        }
    } else {
        win_send_key_event(key.code(), down);
    }
}

#[cfg(windows)]
fn win_send_key_event(keycode: WinKeyCode, down: bool) {
    use winapi::um::winuser::{keybd_event, KEYEVENTF_KEYUP};
    let flags = if down { 0 } else { KEYEVENTF_KEYUP };
    unsafe { keybd_event(keycode as u8, 0, flags, 0) };
}
