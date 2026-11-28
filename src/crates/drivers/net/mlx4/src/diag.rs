//! DAC link diagnostics (#17): the module EEPROM through MAD_IFC (5.11) and
//! the PTYS register through ACCESS_REG (5.12), read-only. Section numbers
//! in comments are docs/spec/connectx3.md's. Every failure here is logged
//! and ignored: diagnostics never stop the data path.

use alloc::string::String;

use crate::fw::Hca;
use crate::hcr::{self, CmdError};
use crate::module::{self, I2C_LOW, PTYS_LEN};
use crate::pci::PciIo;

/// MAD_IFC op_mod: skip the M_Key and B_Key checks, no second block (5.11.1).
const MAD_OP_MOD: u8 = 3;

/// Print `text` one console line per line, each prefixed; on the console
/// when `loud`, else in the trace (#16).
fn print_lines(loud: bool, prefix: &str, text: &str) {
    for line in text.lines() {
        note!(loud, "  {prefix}{line}");
    }
}

/// Run a diagnostic with the HCR in quiet, optional mode: no `ok` lines,
/// and a failed status is this module's to report (#16).
fn optional<T>(hca: &mut Hca, f: impl FnOnce(&mut Hca) -> T) -> T {
    hca.hcr.quiet = true;
    hca.hcr.optional = true;
    let r = f(hca);
    hca.hcr.quiet = false;
    hca.hcr.optional = false;
    r
}

/// One module-info read at I2C 0x50, page 0 (5.11.1–5.11.2). `Ok` is the MAD
/// status (0 = the data is in `out`); `Err` is the HCR failure.
fn read(hca: &mut Hca, pci: &mut PciIo, port: u8, addr: usize, out: &mut [u8]) -> Result<u16, CmdError> {
    let i = &hca.hcr.inbox;
    i.zero();
    i.set_u8(0x00, 1); // base version
    i.set_u8(0x01, 1); // management class
    i.set_u8(0x02, 1); // class version
    i.set_u8(0x03, 1); // method: Get
    i.set_be16(0x10, module::ATTR_MODULE_INFO);
    i.set_u8(0x40, I2C_LOW);
    i.set_u8(0x41, 0); // page
    i.set_be16(0x42, addr as u16);
    i.set_be16(0x46, out.len() as u16);
    hca.hcr.in_out(pci, hcr::MAD_IFC, MAD_OP_MOD, u32::from(port))?;
    let status = hca.hcr.outbox.be16(0x04);
    if status == 0 {
        hca.hcr.outbox.read(0x50, out);
    }
    Ok(status)
}

/// Read and print the module EEPROM of `port` (5.11; spec 7 item 17): the
/// identifier with both statuses, then the raw bytes and their SFF labels.
/// `loud`: on the console even when quiet (a port without link).
/// Returns the port's media for its one line (#31).
pub fn module(hca: &mut Hca, pci: &mut PciIo, port: u8, loud: bool) -> String {
    optional(hca, |hca| module_quiet(hca, pci, port, loud))
}

fn module_quiet(hca: &mut Hca, pci: &mut PciIo, port: u8, loud: bool) -> String {
    let p = alloc::format!("port {port}: module EEPROM: ");
    let mut e = [0u8; 256];
    // The identifier first, 1 byte at device address 0 (5.11.4 item 1).
    let r = read(hca, pci, port, 0, &mut e[..1]);
    let status = match r {
        Ok(s) => s,
        Err(e) => {
            let _ = hca.cmd::<()>(Err(e));
            note!(loud, "  {p}MAD_IFC 0xFF60: {e}; module not readable");
            return "module not readable".into();
        }
    };
    if status != 0 {
        note!(loud, "  {p}HCR status 0, MAD status {status:#06x}: {}", module::mad_error(status));
        return module::media_unread(status).into();
    }
    let id = e[0];
    let Some(kind) = module::kind(id) else {
        note!(loud, "  {p}HCR status 0, MAD status 0, identifier {id:#04x}: not SFP or QSFP; not read further");
        return alloc::format!("module identifier {id:#04x}");
    };
    note!(loud, "  {p}HCR status 0, MAD status 0, identifier {id:#04x} ({kind})");
    let mut text = String::new();
    for &(start, len) in module::ranges(id) {
        for (addr, size) in module::chunks(start, len) {
            let r = read(hca, pci, port, addr, &mut e[addr..addr + size]);
            match r {
                Ok(0) => {}
                Ok(s) => {
                    note!(loud, "  {p}bytes {addr}-{}: MAD status {s:#06x}: {}", addr + size - 1, module::mad_error(s));
                    return alloc::format!("{kind}, EEPROM not fully read");
                }
                Err(e) => {
                    let _ = hca.cmd::<()>(Err(e));
                    note!(loud, "  {p}bytes {addr}-{}: MAD_IFC: {e}", addr + size - 1);
                    return alloc::format!("{kind}, EEPROM not fully read");
                }
            }
        }
        let _ = module::dump(&mut text, &e, start, len);
    }
    let _ = module::describe(&mut text, &e);
    print_lines(loud, &p, &text);
    let mut media = String::new();
    let _ = module::media(&mut media, &e);
    media
}

/// Query PTYS for `port` with ACCESS_REG, read-only (5.12), and print the
/// link-mode masks. `offered` is QUERY_DEV_CAP 0x7a bit 5 (ETH_PROT_CTRL);
/// without it the query is spec 7 item 18's diagnostic, outside what Linux
/// does. Returns whether the firmware answered.
/// `loud` as for `module`.
pub fn ptys(hca: &mut Hca, pci: &mut PciIo, port: u8, offered: bool, loud: bool) -> bool {
    optional(hca, |hca| ptys_quiet(hca, pci, port, offered, loud))
}

fn ptys_quiet(hca: &mut Hca, pci: &mut PciIo, port: u8, offered: bool, loud: bool) -> bool {
    let p = alloc::format!("port {port}: ");
    if !offered {
        note!(loud, "  {p}PTYS: not offered (QUERY_DEV_CAP 0x7a bit 5, ETH_PROT_CTRL, is 0); one read-only query for spec 7 item 18");
    }
    let i = &hca.hcr.inbox;
    i.zero();
    i.set_be16(0x00, 0x0804);
    i.set_be16(0x04, module::REG_PTYS);
    i.set_u8(0x06, 1); // method: query
    i.set_u8(0x07, 0x01);
    i.set_be16(0x10, 0x3000 | (PTYS_LEN / 4 + 1) as u16);
    i.set_u8(0x14 + 1, port); // local port
    i.set_u8(0x14 + 3, module::PROTO_ETH);
    if let Err(e) = hca.hcr.in_out(pci, hcr::ACCESS_REG, 0, 0) {
        let _ = hca.cmd::<()>(Err(e));
        note!(loud, "  {p}PTYS: ACCESS_REG: {e}; no link-mode masks");
        return false;
    }
    let o = &hca.hcr.outbox;
    let status = o.u8(0x02) & 0x7f;
    if status != 0 {
        note!(loud, "  {p}PTYS: HCR status 0, register status {status:#04x}; no link-mode masks");
        return false;
    }
    let mut reg = [0u8; PTYS_LEN];
    o.read(0x14, &mut reg);
    let mut text = String::new();
    let _ = module::describe_ptys(&mut text, &reg);
    print_lines(loud, &p, &text);
    true
}

/// Whether a speed or autonegotiation setting is possible (5.13). The driver
/// never writes PTYS; this only says what the card offers.
/// `loud`: on the console even when quiet (a port without link).
pub fn speed_control(offered: bool, an_rep: bool, loud: bool) {
    if offered {
        note!(
            loud,
            "  speed control: PTYS offered (ETH_PROT_CTRL 1, ETH_BACKPL_AN_REP {}); this driver does not force a speed (5.13)",
            an_rep as u8
        );
    } else {
        note!(
            loud,
            "  speed control: not offered (ETH_PROT_CTRL 0): no forced speed or autoneg setting on this card (5.13); set the switch port"
        );
    }
}
