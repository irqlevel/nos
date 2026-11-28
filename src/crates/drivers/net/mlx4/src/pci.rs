use alloc::sync::Arc;

use kcore::{dma, io, pci};

use crate::bars;

pub const ATTR_MEMORY: u64 = 0x0200;
pub const ATTR_BUS_MASTER: u64 = 0x0400;
pub const ATTR_DUAL_ADDRESS_CYCLE: u64 = 0x8000;

const DCS_PAGES: usize = 256;
const UAR_PAGES: usize = 2048;
const COMMAND: u16 = 0x04;

#[derive(Debug)]
pub struct Status;

#[derive(Debug)]
pub struct Error;

impl Error {
    pub fn status(&self) -> Status {
        Status
    }
}

pub type Result<T> = core::result::Result<T, Error>;

struct Shared {
    dev: pci::PciDevice,
    _dcs_mapping: dma::PhysMapping,
    _uar_mapping: dma::PhysMapping,
    dcs: io::MmioRegion,
    uar: io::MmioRegion,
}

#[derive(Clone)]
pub struct PciIo {
    shared: Arc<Shared>,
}

impl PciIo {
    pub fn new(dev: &pci::PciDevice) -> Result<Self> {
        dev.enable_bus_mastering();
        let dcs_phys = dev.bar_address(0);
        let uar_phys = dev.bar_address(2);
        if dcs_phys == 0 || uar_phys == 0 {
            return Err(Error);
        }
        let dcs_mapping = dma::PhysMapping::map(dcs_phys, DCS_PAGES).ok_or(Error)?;
        let uar_mapping = dma::PhysMapping::map(uar_phys, UAR_PAGES).ok_or(Error)?;
        let dcs = io::MmioRegion::new(dcs_mapping.as_mut_ptr(), DCS_PAGES * 4096);
        let uar = io::MmioRegion::new(uar_mapping.as_mut_ptr(), UAR_PAGES * 4096);
        Ok(Self {
            shared: Arc::new(Shared {
                dev: *dev,
                _dcs_mapping: dcs_mapping,
                _uar_mapping: uar_mapping,
                dcs,
                uar,
            }),
        })
    }

    pub fn device(&self) -> pci::PciDevice {
        self.shared.dev
    }

    pub fn read_config32(&mut self, offset: u32) -> Result<u32> {
        Ok(self.shared.dev.read_config32(u16::try_from(offset).map_err(|_| Error)?))
    }

    pub fn write_config32(&mut self, offset: u32, value: u32) -> Result<()> {
        self.shared.dev.write_config32(u16::try_from(offset).map_err(|_| Error)?, value);
        Ok(())
    }

    pub fn read_config16(&mut self, offset: u32) -> Result<u16> {
        Ok(self.shared.dev.read_config16(u16::try_from(offset).map_err(|_| Error)?))
    }

    pub fn write_config16(&mut self, offset: u32, value: u16) -> Result<()> {
        self.shared.dev.write_config16(u16::try_from(offset).map_err(|_| Error)?, value);
        Ok(())
    }

    fn region(&self, bar: u8) -> Result<&io::MmioRegion> {
        match bar {
            0 => Ok(&self.shared.dcs),
            2 => Ok(&self.shared.uar),
            _ => Err(Error),
        }
    }

    fn region_len(&self, bar: u8) -> Result<usize> {
        match bar {
            0 => Ok(DCS_PAGES * 4096),
            2 => Ok(UAR_PAGES * 4096),
            _ => Err(Error),
        }
    }

    pub fn mem_read32(&mut self, bar: u8, offset: u64) -> Result<u32> {
        let offset = usize::try_from(offset).map_err(|_| Error)?;
        let region_len = self.region_len(bar)?;
        if offset.checked_add(4).is_none_or(|end| end > region_len) {
            return Err(Error);
        }
        Ok(self.region(bar)?.read32(offset))
    }

    pub fn mem_write32(&mut self, bar: u8, offset: u64, value: u32) -> Result<()> {
        let offset = usize::try_from(offset).map_err(|_| Error)?;
        let region_len = self.region_len(bar)?;
        if offset.checked_add(4).is_none_or(|end| end > region_len) {
            return Err(Error);
        }
        self.region(bar)?.write32(offset, value);
        Ok(())
    }

    pub fn get_attributes(&mut self) -> Result<u64> {
        Ok(u64::from(self.shared.dev.read_config16(COMMAND)))
    }

    pub fn supported_attributes(&mut self) -> Result<u64> {
        Ok(ATTR_DUAL_ADDRESS_CYCLE)
    }

    pub fn enable_attributes(&mut self, attributes: u64) -> Result<u64> {
        let mut command = self.shared.dev.read_config16(COMMAND);
        if attributes & ATTR_MEMORY != 0 {
            command |= 1 << 1;
        }
        if attributes & ATTR_BUS_MASTER != 0 {
            command |= 1 << 2;
        }
        self.shared.dev.write_config16(COMMAND, command);
        Ok(attributes)
    }

    pub fn set_attributes(&mut self, attributes: u64) -> Result<()> {
        self.shared.dev.write_config16(COMMAND, attributes as u16);
        Ok(())
    }

    pub fn find_bar(&mut self, reg: usize) -> Result<bars::Pick> {
        let index = u8::try_from(reg).map_err(|_| Error)?;
        let size = self.region_len(index)? as u64;
        Ok(bars::Pick { index, size: Some(size), how: bars::How::BaseMatches })
    }
}
