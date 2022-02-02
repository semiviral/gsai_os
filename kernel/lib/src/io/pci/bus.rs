use crate::{Physical, io::pci::DeviceVariant, Address};
use alloc::vec::Vec;

pub struct PCIeBus {
    devices: Vec<DeviceVariant>,
}

impl PCIeBus {
    pub unsafe fn new(base_addr: Address<Physical>) -> Self {
        let devices: Vec<DeviceVariant> = (0..32)
            .filter_map(|device_index| {
                let offset_addr = base_addr + (device_index << 15);
                let vendor_id = (crate::memory::get_page_manager().mapped_addr()
                    + offset_addr.as_usize())
                .as_ptr::<u16>()
                .read();

                if vendor_id == u16::MAX || vendor_id == u16::MIN {
                    None
                } else {
                    trace!("Configuring PCIe device at {:?}", offset_addr);

                    Some(crate::io::pci::new_device(
                        crate::memory::MMIO::new(offset_addr.frame_index(), 1).expect(
                            "Allocation error occurred attempting to create MMIO for PCIeBus",
                        ),
                    ))
                }
            })
            .collect();

        Self { devices }
    }

    pub fn has_devices(&self) -> bool {
        self.devices.len() > 0
    }

    pub fn iter(&self) -> core::slice::Iter<DeviceVariant> {
        self.devices.iter()
    }

    pub fn iter_mut(&mut self) -> core::slice::IterMut<DeviceVariant> {
        self.devices.iter_mut()
    }
}

impl core::fmt::Debug for PCIeBus {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PCIeBus")
            .field("Devices", &self.devices)
            .finish()
    }
}
