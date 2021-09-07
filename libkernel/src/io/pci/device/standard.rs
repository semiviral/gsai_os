use crate::{
    io::pci::{
        PCIeDevice, PCIeDeviceRegister, PCIeDeviceRegisterIterator, PCIeDeviceType, Standard,
    },
    memory::mmio::{Mapped, MMIO},
};
use core::fmt;

/// An exaplanation of the acronyms used here can be inferred from:
///  https://lekensteyn.nl/files/docs/PCI_SPEV_V3_0.pdf table H-1
#[derive(Debug)]
pub enum PCICapablities {
    /// PCI Power Management Interface
    PWMI,
    /// Accelerated Graphics Port
    AGP,
    /// Vital Product Data
    VPD,
    /// Slot Identification
    SIDENT,
    /// Message Signaled Interrupts
    MSI,
    /// CompactPCI Hot Swap
    CPCIHS,
    /// PCI-X
    PCIX,
    /// HyperTransport
    HYTPT,
    /// Vendor Specific
    VENDOR,
    /// Debug Port
    DEBUG,
    /// CompactPCI Central Resource Control
    CPCICPC,
    /// PCI Hot-Plug
    HOTPLG,
    /// PCI Bridge Subsystem Vendor ID
    SSYSVENDID,
    /// AGP 8x
    AGP8X,
    /// Secure Device
    SECURE,
    /// PCI Express
    PCIE,
    /// Message Signaled Interrupt Extension
    MSIX,
    Reserved,
    NotImplemented,
}

pub struct PCICapablitiesIterator<'mmio> {
    mmio: &'mmio MMIO<Mapped>,
    offset: u8,
}

impl<'mmio> PCICapablitiesIterator<'mmio> {
    fn new(mmio: &'mmio MMIO<Mapped>, offset: u8) -> Self {
        Self { mmio, offset }
    }
}

impl Iterator for PCICapablitiesIterator<'_> {
    type Item = PCICapablities;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset > 0 {
            unsafe {
                use bit_field::BitField;

                let cap_reg_00 = self.mmio.read::<u32>(self.offset as usize).unwrap().read();
                self.offset = cap_reg_00.get_bits(8..16) as u8;

                Some(match cap_reg_00.get_bits(0..8) {
                    0x1 => PCICapablities::PWMI,
                    0x2 => PCICapablities::AGP,
                    0x3 => PCICapablities::VPD,
                    0x4 => PCICapablities::SIDENT,
                    0x5 => PCICapablities::MSI,
                    0x6 => PCICapablities::CPCIHS,
                    0x7 => PCICapablities::PCIX,
                    0x8 => PCICapablities::HYTPT,
                    0x9 => PCICapablities::VENDOR,
                    0xA => PCICapablities::DEBUG,
                    0xB => PCICapablities::CPCICPC,
                    0xC => PCICapablities::HOTPLG,
                    0xD => PCICapablities::SSYSVENDID,
                    0xE => PCICapablities::AGP8X,
                    0xF => PCICapablities::SECURE,
                    0x10 => PCICapablities::PCIE,
                    0x11 => PCICapablities::MSIX,
                    0x0 | 0x12..0xFF => PCICapablities::Reserved,
                    _ => PCICapablities::NotImplemented,
                })
            }
        } else {
            None
        }
    }
}

#[repr(usize)]
#[derive(Debug)]
pub enum StandardRegister {
    Register0 = 0,
    Register1 = 1,
    Register2 = 2,
    Register3 = 3,
    Register4 = 4,
    Register5 = 5,
}

impl PCIeDevice<Standard> {
    pub unsafe fn new(mmio: MMIO<Mapped>) -> Self {
        assert_eq!(
            (mmio
                .read::<u8>(crate::io::pci::PCIHeaderOffset::HeaderType.into())
                .unwrap()
                .read())
                & !(1 << 7),
            0,
            "incorrect header type for standard specification PCI device"
        );

        let mut registers = alloc::vec![None, None, None, None, None];

        for (register_num, register) in PCIeDeviceRegisterIterator::new(
            mmio.mapped_addr().as_mut_ptr::<u32>().add(0x4),
            Standard::REGISTER_COUNT,
        )
        .filter(|register| !register.is_unused())
        .enumerate()
        {
            debug!("Device Register {}: {:?}", register_num, register);

            // The address is MMIO, so is memory-mapped—thus,
            //  the page index and frame index will match.
            let frame_index = register.as_addr().page_index();
            let frame_usage = crate::align_up_div(register.memory_usage(), 0x1000);
            debug!(
                "\tAcquiring register destination frame as MMIO: {}:{}",
                frame_index, frame_usage
            );
            let mmio_frames = crate::memory::falloc::get()
                .acquire_frames(
                    frame_index,
                    frame_usage,
                    crate::memory::falloc::FrameState::MMIO,
                )
                .expect("frames are not MMIO");
            debug!("\tAuto-mapping register destination frame.");
            let register_mmio = crate::memory::mmio::unmapped_mmio(mmio_frames)
                .expect("failed to create MMIO object")
                .automap();

            if match register {
                PCIeDeviceRegister::MemorySpace32(value, _) => (value & 0b1000) > 0,
                PCIeDeviceRegister::MemorySpace64(value, _) => (value & 0b1000) > 0,
                _ => false,
            } {
                debug!("\tRegister is prefetchable, so enabling WRITE_THROUGH bit on page.");
                // Optimize page attributes to enable write-through if it wasn't previously enabled.
                for page in register_mmio.pages() {
                    use crate::memory::{
                        malloc::get,
                        paging::{PageAttributeModifyMode, PageAttributes},
                    };

                    get().modify_page_attributes(
                        &page,
                        PageAttributes::WRITE_THROUGH,
                        PageAttributeModifyMode::Insert,
                    );

                    get().modify_page_attributes(
                        &page,
                        PageAttributes::UNCACHEABLE,
                        PageAttributeModifyMode::Remove,
                    );
                }
            }

            registers[register_num] = Some(register_mmio);
        }

        Self {
            mmio,
            registers,
            phantom: core::marker::PhantomData,
        }
    }

    pub fn cardbus_cis_ptr(&self) -> u32 {
        unsafe { self.mmio.read(0x28).unwrap().read() }
    }

    pub fn subsystem_vendor_id(&self) -> u16 {
        unsafe { self.mmio.read(0x2C).unwrap().read() }
    }

    pub fn subsystem_id(&self) -> u16 {
        unsafe { self.mmio.read(0x2E).unwrap().read() }
    }

    pub fn expansion_rom_base_addr(&self) -> u32 {
        unsafe { self.mmio.read(0x30).unwrap().read() }
    }

    pub fn capabilities(&self) -> PCICapablitiesIterator {
        PCICapablitiesIterator::new(&self.mmio, unsafe {
            self.mmio.read::<u8>(0x34).unwrap().read() & !0b11
        })
    }

    pub fn interrupt_line(&self) -> Option<u8> {
        match unsafe { self.mmio.read(0x3C).unwrap().read() } {
            0xFF => None,
            value => Some(value),
        }
    }

    pub fn interrupt_pin(&self) -> Option<u8> {
        match unsafe { self.mmio.read(0x3D).unwrap().read() } {
            0x0 => None,
            value => Some(value),
        }
    }

    pub fn min_grant(&self) -> u8 {
        unsafe { self.mmio.read(0x3E).unwrap().read() }
    }

    pub fn max_latency(&self) -> u8 {
        unsafe { self.mmio.read(0x3F).unwrap().read() }
    }

    pub fn iter_registers(&self) -> core::slice::Iter<Option<MMIO<Mapped>>> {
        self.registers.iter()
    }
}

impl core::ops::Index<StandardRegister> for PCIeDevice<Standard> {
    type Output = Option<MMIO<Mapped>>;

    fn index(&self, index: StandardRegister) -> &Self::Output {
        &self.registers[index as usize]
    }
}

impl fmt::Debug for PCIeDevice<Standard> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let debug_struct = &mut formatter.debug_struct("PCIe Device (Standard)");

        self.generic_debut_fmt(debug_struct);
        debug_struct
            .field("Cardbus CIS Pointer", &self.cardbus_cis_ptr())
            .field("Subsystem Vendor ID", &self.subsystem_vendor_id())
            .field("Subsystem ID", &self.subsystem_id())
            .field(
                "Expansion ROM Base Address",
                &self.expansion_rom_base_addr(),
            )
            .field("Interrupt Line", &self.interrupt_line())
            .field("Interrupt Pin", &self.interrupt_pin())
            .field("Min Grant", &self.min_grant())
            .field("Max Latency", &self.max_latency())
            .finish()
    }
}
