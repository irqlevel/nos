//! The HID boot-protocol keyboard: eight bytes a report,
//!
//! ```text
//!     byte 0   modifier bitmap (L/R Ctrl, Shift, Alt, GUI)
//!     byte 1   reserved
//!     byte 2-7 up to six concurrently pressed usage codes
//! ```
//!
//! Reports are level, not edge: a key is "down" for as long as its usage
//! stays in the array. Presses are therefore found by diffing against the
//! previous report, and auto-repeat is synthesised here because a boot
//! keyboard never repeats on its own.

use kcore::trace;

/// The per-subsystem trace level, as `kernel/trace.h` has it for KbdLL.
const KBD_LL: u32 = 3;

pub const REPORT_SIZE: usize = 8;

/// The HID Keyboard/Keypad usage page (0x07) against a character and the
/// PS/2 set-1 make code the rest of the kernel already speaks. Consumers such
/// as the shell key off the scan code (0x0E is backspace), so keeping the
/// legacy codes makes a USB keyboard indistinguishable from the 8042 one. A
/// zero character means "no printable character" and is never delivered.
struct Key {
    normal: u8,
    shifted: u8,
    ps2: u8,
}

const fn k(normal: u8, shifted: u8, ps2: u8) -> Key {
    Key { normal, shifted, ps2 }
}

#[rustfmt::skip]
static KEYMAP: &[Key] = &[
    /* 0x00 */ k(0, 0, 0x00),      /* Reserved / no event      */
    /* 0x01 */ k(0, 0, 0x00),      /* ErrorRollOver            */
    /* 0x02 */ k(0, 0, 0x00),      /* POSTFail                 */
    /* 0x03 */ k(0, 0, 0x00),      /* ErrorUndefined           */
    /* 0x04 */ k(b'a', b'A', 0x1E),
    /* 0x05 */ k(b'b', b'B', 0x30),
    /* 0x06 */ k(b'c', b'C', 0x2E),
    /* 0x07 */ k(b'd', b'D', 0x20),
    /* 0x08 */ k(b'e', b'E', 0x12),
    /* 0x09 */ k(b'f', b'F', 0x21),
    /* 0x0A */ k(b'g', b'G', 0x22),
    /* 0x0B */ k(b'h', b'H', 0x23),
    /* 0x0C */ k(b'i', b'I', 0x17),
    /* 0x0D */ k(b'j', b'J', 0x24),
    /* 0x0E */ k(b'k', b'K', 0x25),
    /* 0x0F */ k(b'l', b'L', 0x26),
    /* 0x10 */ k(b'm', b'M', 0x32),
    /* 0x11 */ k(b'n', b'N', 0x31),
    /* 0x12 */ k(b'o', b'O', 0x18),
    /* 0x13 */ k(b'p', b'P', 0x19),
    /* 0x14 */ k(b'q', b'Q', 0x10),
    /* 0x15 */ k(b'r', b'R', 0x13),
    /* 0x16 */ k(b's', b'S', 0x1F),
    /* 0x17 */ k(b't', b'T', 0x14),
    /* 0x18 */ k(b'u', b'U', 0x16),
    /* 0x19 */ k(b'v', b'V', 0x2F),
    /* 0x1A */ k(b'w', b'W', 0x11),
    /* 0x1B */ k(b'x', b'X', 0x2D),
    /* 0x1C */ k(b'y', b'Y', 0x15),
    /* 0x1D */ k(b'z', b'Z', 0x2C),
    /* 0x1E */ k(b'1', b'!', 0x02),
    /* 0x1F */ k(b'2', b'@', 0x03),
    /* 0x20 */ k(b'3', b'#', 0x04),
    /* 0x21 */ k(b'4', b'$', 0x05),
    /* 0x22 */ k(b'5', b'%', 0x06),
    /* 0x23 */ k(b'6', b'^', 0x07),
    /* 0x24 */ k(b'7', b'&', 0x08),
    /* 0x25 */ k(b'8', b'*', 0x09),
    /* 0x26 */ k(b'9', b'(', 0x0A),
    /* 0x27 */ k(b'0', b')', 0x0B),
    /* 0x28 */ k(b'\n', b'\n', 0x1C),   /* Enter        */
    /* 0x29 */ k(0, 0, 0x01),           /* Escape       */
    /* 0x2A */ k(8, 8, 0x0E),           /* Backspace    */
    /* 0x2B */ k(b'\t', b'\t', 0x0F),   /* Tab          */
    /* 0x2C */ k(b' ', b' ', 0x39),     /* Space        */
    /* 0x2D */ k(b'-', b'_', 0x0C),
    /* 0x2E */ k(b'=', b'+', 0x0D),
    /* 0x2F */ k(b'[', b'{', 0x1A),
    /* 0x30 */ k(b']', b'}', 0x1B),
    /* 0x31 */ k(b'\\', b'|', 0x2B),
    /* 0x32 */ k(b'\\', b'|', 0x2B),    /* non-US # / ~ */
    /* 0x33 */ k(b';', b':', 0x27),
    /* 0x34 */ k(b'\'', b'"', 0x28),
    /* 0x35 */ k(b'`', b'~', 0x29),
    /* 0x36 */ k(b',', b'<', 0x33),
    /* 0x37 */ k(b'.', b'>', 0x34),
    /* 0x38 */ k(b'/', b'?', 0x35),
    /* 0x39 */ k(0, 0, 0x3A),           /* CapsLock     */
    /* 0x3A */ k(0, 0, 0x3B),           /* F1           */
    /* 0x3B */ k(0, 0, 0x3C),
    /* 0x3C */ k(0, 0, 0x3D),
    /* 0x3D */ k(0, 0, 0x3E),
    /* 0x3E */ k(0, 0, 0x3F),
    /* 0x3F */ k(0, 0, 0x40),
    /* 0x40 */ k(0, 0, 0x41),
    /* 0x41 */ k(0, 0, 0x42),
    /* 0x42 */ k(0, 0, 0x43),
    /* 0x43 */ k(0, 0, 0x44),           /* F10          */
    /* 0x44 */ k(0, 0, 0x57),           /* F11          */
    /* 0x45 */ k(0, 0, 0x58),           /* F12          */
    /* 0x46 */ k(0, 0, 0x00),           /* PrintScreen  */
    /* 0x47 */ k(0, 0, 0x46),           /* ScrollLock   */
    /* 0x48 */ k(0, 0, 0x00),           /* Pause        */
    /* 0x49 */ k(0, 0, 0x00),           /* Insert       */
    /* 0x4A */ k(0, 0, 0x00),           /* Home         */
    /* 0x4B */ k(0, 0, 0x00),           /* PageUp       */
    /* 0x4C */ k(0, 0, 0x00),           /* Delete       */
    /* 0x4D */ k(0, 0, 0x00),           /* End          */
    /* 0x4E */ k(0, 0, 0x00),           /* PageDown     */
    /* 0x4F */ k(0, 0, 0x00),           /* Right arrow  */
    /* 0x50 */ k(0, 0, 0x00),           /* Left arrow   */
    /* 0x51 */ k(0, 0, 0x00),           /* Down arrow   */
    /* 0x52 */ k(0, 0, 0x00),           /* Up arrow     */
    /* 0x53 */ k(0, 0, 0x45),           /* NumLock      */
    /* 0x54 */ k(b'/', b'/', 0x35),     /* Keypad /     */
    /* 0x55 */ k(b'*', b'*', 0x37),
    /* 0x56 */ k(b'-', b'-', 0x4A),
    /* 0x57 */ k(b'+', b'+', 0x4E),
    /* 0x58 */ k(b'\n', b'\n', 0x1C),   /* Keypad Enter */
    /* 0x59 */ k(b'1', b'1', 0x4F),
    /* 0x5A */ k(b'2', b'2', 0x50),
    /* 0x5B */ k(b'3', b'3', 0x51),
    /* 0x5C */ k(b'4', b'4', 0x4B),
    /* 0x5D */ k(b'5', b'5', 0x4C),
    /* 0x5E */ k(b'6', b'6', 0x4D),
    /* 0x5F */ k(b'7', b'7', 0x47),
    /* 0x60 */ k(b'8', b'8', 0x48),
    /* 0x61 */ k(b'9', b'9', 0x49),
    /* 0x62 */ k(b'0', b'0', 0x52),
    /* 0x63 */ k(b'.', b'.', 0x53),
    /* 0x64 */ k(b'\\', b'|', 0x56),    /* non-US \ / | */
];

const MOD_LEFT_SHIFT: u8 = 1 << 1;
const MOD_RIGHT_SHIFT: u8 = 1 << 5;

const USAGE_ERROR_ROLLOVER: u8 = 0x01;
const USAGE_CAPS_LOCK: u8 = 0x39;

/// Auto-repeat: the hold before the first repeat, then the period.
const REPEAT_DELAY_NS: u64 = 400 * 1000 * 1000;
const REPEAT_PERIOD_NS: u64 = 40 * 1000 * 1000;

pub struct BootKeyboard {
    prev: [u8; REPORT_SIZE],
    caps_lock: bool,
    /// 0 when nothing is held.
    repeat_usage: u8,
    repeat_modifiers: u8,
    repeat_next_ns: u64,
}

impl BootKeyboard {
    pub const fn new() -> Self {
        Self {
            prev: [0; REPORT_SIZE],
            caps_lock: false,
            repeat_usage: 0,
            repeat_modifiers: 0,
            repeat_next_ns: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn in_report(report: &[u8], usage: u8) -> bool {
        report[2..REPORT_SIZE].contains(&usage)
    }

    /// True when the usage produced a character, and is therefore eligible
    /// for auto-repeat.
    fn emit(&mut self, usage: u8, modifiers: u8) -> bool {
        let entry = match KEYMAP.get(usage as usize) {
            Some(entry) => entry,
            None => return false,
        };

        if usage == USAGE_CAPS_LOCK {
            self.caps_lock = !self.caps_lock;
            return false;
        }

        let shift = modifiers & (MOD_LEFT_SHIFT | MOD_RIGHT_SHIFT) != 0;
        let mut c = if shift { entry.shifted } else { entry.normal };

        /* Caps Lock affects letters only, and inverts the effect of Shift */
        if self.caps_lock && entry.normal.is_ascii_lowercase() {
            c = if shift { entry.normal } else { entry.shifted };
        }

        if c == 0 {
            return false;
        }

        trace!(KBD_LL, "UsbKbd: usage 0x{:X} char {}", usage, c as char);
        kcore::input::key(c, entry.ps2);
        true
    }

    /// Feed one report. Emits decoded characters into the kernel's input.
    pub fn on_report(&mut self, report: &[u8]) {
        if report.len() < REPORT_SIZE {
            return;
        }

        /* The keyboard reports a rollover condition by filling every slot
         * with ErrorRollOver; there is no key information in such a report. */
        if report[2] == USAGE_ERROR_ROLLOVER {
            return;
        }

        let modifiers = report[0];
        let mut last_pressed = 0;

        for i in 2..REPORT_SIZE {
            let usage = report[i];
            if usage == 0 {
                continue;
            }
            if !Self::in_report(&self.prev, usage) {
                /* Only character-producing keys arm auto-repeat: repeating a
                 * lock key would toggle it dozens of times a second. */
                if self.emit(usage, modifiers) {
                    last_pressed = usage;
                }
            }
        }

        self.prev.copy_from_slice(&report[..REPORT_SIZE]);

        if last_pressed != 0 {
            /* Newest press wins the repeat, matching every other keyboard */
            self.repeat_usage = last_pressed;
            self.repeat_modifiers = modifiers;
            self.repeat_next_ns = 0;
        } else if self.repeat_usage != 0 && !Self::in_report(&self.prev, self.repeat_usage) {
            self.repeat_usage = 0;
        }
    }

    /// Drive auto-repeat; called on every poll tick with the boot-time clock.
    pub fn on_tick(&mut self, now_ns: u64) {
        if self.repeat_usage == 0 {
            return;
        }

        if self.repeat_next_ns == 0 {
            self.repeat_next_ns = now_ns + REPEAT_DELAY_NS;
            return;
        }

        if now_ns < self.repeat_next_ns {
            return;
        }

        let (usage, modifiers) = (self.repeat_usage, self.repeat_modifiers);
        self.emit(usage, modifiers);
        self.repeat_next_ns = now_ns + REPEAT_PERIOD_NS;
    }
}
