use crate::memory::{
    allocators::{global_memory_mut, GlobalAllocator},
    paging::VirtualAddessor,
    Page,
};
use core::cell::RefCell;
use x86_64::VirtAddr;

pub struct BumpAllocator<'vaddr> {
    virtual_addessor: RefCell<&'vaddr mut dyn VirtualAddessor>,
    bottom_page: RefCell<Page>,
}

impl<'vaddr> BumpAllocator<'vaddr> {
    pub fn new(virtual_addessor: &'vaddr mut dyn VirtualAddessor) -> Self {
        Self {
            virtual_addessor: RefCell::new(virtual_addessor),
            bottom_page: RefCell::new(Page::from_addr(VirtAddr::new(0x1000))),
        }
    }
}

unsafe impl GlobalAllocator for BumpAllocator<'_> {
    unsafe fn alloc(&self, size: usize) -> VirtAddr {
        let bottom_addr = self.bottom_page.borrow().addr();

        let start_addr_usize = bottom_addr.as_u64() as usize;
        let end_addr_usize = efi_boot::align_down(start_addr_usize + size, 0x1000);
        for addr in (start_addr_usize..end_addr_usize).step_by(0x1000) {
            self.virtual_addessor.borrow_mut().map(
                &Page::from_addr(VirtAddr::new(addr as u64)),
                &global_memory_mut(|allocator| {
                    allocator.lock_next().expect("failed to allocate frames")
                }),
            );
        }

        self.bottom_page.replace(Page::from_addr(
            bottom_addr + (efi_boot::align_up(size, 0x1000) as u64),
        ));

        bottom_addr
    }

    unsafe fn dealloc(&self, _: VirtAddr, __: usize) {}
}
