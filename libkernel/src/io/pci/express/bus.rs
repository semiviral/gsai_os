use crate::{addr_ty::Physical, io::pci::express::PCIeDevice, Address};
use alloc::vec::Vec;

pub struct PCIeBus {
    devices: Option<Vec<PCIeDevice>>,
}

impl PCIeBus {
    pub unsafe fn new(base_addr: Address<Physical>) -> Self {
        let devices: Vec<PCIeDevice> = (0..32)
            .filter_map(|device_index| {
                let offset_addr = base_addr + (device_index << 15);
                let header = &*crate::memory::malloc::get()
                    .physical_memory(offset_addr)
                    .as_ptr::<crate::io::pci::PCIDeviceHeader>();

                if header.is_valid() {
                    debug!(
                        "Found PCIe device: {} {} [0x{:X}:0x{:X}]",
                        header.vendor_str(),
                        header.device_str(),
                        header.vendor_id(),
                        header.device_id()
                    );

                    let mmio_frames = crate::memory::falloc::get()
                        .acquire_frame(
                            offset_addr.frame_index(),
                            crate::memory::falloc::FrameState::MMIO,
                        )
                        .unwrap()
                        .into_iter();

                    Some(PCIeDevice::new(
                        crate::memory::mmio::unmapped_mmio(mmio_frames)
                            .unwrap()
                            .map(),
                    ))
                } else {
                    None
                }
            })
            .collect();

        Self {
            devices: {
                if devices.len() > 0 {
                    Some(devices)
                } else {
                    None
                }
            },
        }
    }

    pub const fn is_valid(&self) -> bool {
        self.devices.is_some()
    }

    pub fn iter_devices(&self) -> core::slice::Iter<PCIeDevice> {
        self.devices.as_ref().expect("bus not configured").iter()
    }

    pub fn iter_mut(&mut self) -> core::slice::IterMut<PCIeDevice> {
        self.devices
            .as_mut()
            .expect("but not configured")
            .iter_mut()
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
