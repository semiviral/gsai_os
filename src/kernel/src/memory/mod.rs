mod paging;

pub mod io;
use libsys::{page_size, table_index_size};
pub use paging::*;
pub mod address_space;
pub mod pmm;

use crate::{exceptions::Exception, interrupts::InterruptCell, local_state::do_catch};
use address_space::Mapper;
use alloc::{alloc::Global, string::String};
use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::NonNull,
};
use libsys::{Address, Frame, Virtual};
use slab::SlabAllocator;
use spin::{Lazy, Mutex, Once};
use try_alloc::boxed::TryBox;

pub fn hhdm_address() -> Address<Virtual> {
    static HHDM_ADDRESS: Once<Address<Virtual>> = Once::new();

    *HHDM_ADDRESS.call_once(|| {
        static LIMINE_HHDM: limine::LimineHhdmRequest = limine::LimineHhdmRequest::new(crate::boot::LIMINE_REV);

        let offset =
            LIMINE_HHDM.get_response().get().expect("bootloader provided no higher-half direct mapping").offset;

        Address::new(offset as usize).expect("bootloader provided a non-canonical higher-half direct mapping address")
    })
}

pub unsafe fn hhdm_offset(frame: Address<Frame>) -> Option<Address<libsys::Page>> {
    Address::new(hhdm_address().as_ptr().add(frame.get().get()).addr())
}

pub fn with_kmapper<T>(func: impl FnOnce(&mut Mapper) -> T) -> T {
    static KERNEL_MAPPER: Once<InterruptCell<Mutex<Mapper>>> = Once::new();

    KERNEL_MAPPER
        .call_once(|| {
            debug!("Creating kernel-space address mapper.");

            InterruptCell::new(Mutex::new(Mapper::new(PageDepth::current()).unwrap()))
        })
        .with(|mapper| {
            let mut mapper = mapper.lock();
            func(&mut *mapper)
        })
}

pub fn new_kmapped_page_table() -> Option<Address<Frame>> {
    let table_frame = PMM.next_frame().ok()?;

    // Safety: Frame is provided by allocator, and so guaranteed to be within the HHDM, and is frame-sized.
    let new_table = unsafe {
        core::slice::from_raw_parts_mut(
            hhdm_offset(table_frame).unwrap().as_ptr().cast::<PageTableEntry>(),
            table_index_size().get(),
        )
    };
    new_table.fill(PageTableEntry::empty());
    with_kmapper(|kmapper| new_table.copy_from_slice(kmapper.view_root_page_table()));

    Some(table_frame)
}

#[cfg(target_arch = "x86_64")]
pub struct PagingRegister(pub Address<Frame>, pub crate::arch::x64::registers::control::CR3Flags);
#[cfg(target_arch = "riscv64")]
pub struct PagingRegister(pub Address<Frame>, pub u16, pub crate::arch::rv64::registers::satp::Mode);

impl PagingRegister {
    pub fn read() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            let args = crate::arch::x64::registers::control::CR3::read();
            Self(args.0, args.1)
        }

        #[cfg(target_arch = "riscv64")]
        {
            let args = crate::arch::rv64::registers::satp::read();
            Self(args.0, args.1, args.2)
        }
    }

    /// ### Safety
    ///
    /// Writing to this register has the chance to externally invalidate memory references.
    pub unsafe fn write(args: &Self) {
        #[cfg(target_arch = "x86_64")]
        crate::arch::x64::registers::control::CR3::write(args.0, args.1);

        #[cfg(target_arch = "riscv64")]
        crate::arch::rv64::registers::satp::write(args.0.as_usize(), args.1, args.2);
    }

    #[inline]
    pub const fn frame(&self) -> Address<Frame> {
        self.0
    }
}

pub fn supports_5_level_paging() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        crate::arch::x64::cpuid::EXT_FEATURE_INFO
            .as_ref()
            .map(|ext_feature_info| ext_feature_info.has_la57())
            .unwrap_or(false)
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!()
    }
}

pub fn is_5_level_paged() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        supports_5_level_paging()
            && crate::arch::x64::registers::control::CR4::read()
                .contains(crate::arch::x64::registers::control::CR4Flags::LA57)
    }
}

pub fn current_paging_levels() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_5_level_paged() {
            5
        } else {
            4
        }
    }
}

pub type PhysicalAllocator = &'static pmm::PhysicalMemoryManager<'static>;

pub static PMM: Lazy<pmm::PhysicalMemoryManager> = Lazy::new(|| unsafe {
    let memory_map = crate::boot::get_memory_map().unwrap();
    pmm::PhysicalMemoryManager::from_memory_map(
        memory_map.iter().map(|entry| pmm::MemoryMapping {
            base: entry.base as usize,
            len: entry.len as usize,
            typ: {
                use limine::LimineMemoryMapEntryType;
                use pmm::FrameType;

                match entry.typ {
                    LimineMemoryMapEntryType::Usable => FrameType::Generic,
                    LimineMemoryMapEntryType::BootloaderReclaimable => FrameType::BootReclaim,
                    LimineMemoryMapEntryType::AcpiReclaimable => FrameType::AcpiReclaim,
                    LimineMemoryMapEntryType::KernelAndModules
                    | LimineMemoryMapEntryType::Reserved
                    | LimineMemoryMapEntryType::AcpiNvs
                    | LimineMemoryMapEntryType::Framebuffer => FrameType::Reserved,
                    LimineMemoryMapEntryType::BadMemory => FrameType::Unusable,
                }
            },
        }),
        hhdm_address(),
    )
    .unwrap()
});

pub static KMALLOC: Lazy<SlabAllocator<&pmm::PhysicalMemoryManager>> = Lazy::new(|| SlabAllocator::new_in(11, &*PMM));

mod global_allocator_impl {
    use super::KMALLOC;
    use core::{
        alloc::{Allocator, GlobalAlloc, Layout},
        ptr::NonNull,
    };

    struct GlobalAllocator;

    unsafe impl GlobalAlloc for GlobalAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            KMALLOC.allocate(layout).map_or(core::ptr::null_mut(), |ptr| ptr.as_non_null_ptr().as_ptr())
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            KMALLOC.deallocate(NonNull::new(ptr).unwrap(), layout);
        }
    }

    unsafe impl Allocator for GlobalAllocator {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, core::alloc::AllocError> {
            KMALLOC.allocate(layout)
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            KMALLOC.deallocate(ptr, layout);
        }
    }

    #[global_allocator]
    static GLOBAL_ALLOCATOR: GlobalAllocator = GlobalAllocator;
}

pub unsafe fn out_of_memory() -> ! {
    panic!("Kernel ran out of memory during initialization.")
}

pub type Stack = TryBox<[core::mem::MaybeUninit<u8>], AlignedAllocator<0x10>>;

pub fn allocate_kernel_stack<const SIZE: usize>() -> Result<Stack, AllocError> {
    TryBox::new_uninit_slice_in(SIZE, AlignedAllocator::new())
}

pub struct AlignedAllocator<const ALIGN: usize, A: Allocator = Global>(A);

impl<const ALIGN: usize> AlignedAllocator<ALIGN> {
    #[inline]
    pub const fn new() -> Self {
        AlignedAllocator::new_in(Global)
    }
}

impl<const ALIGN: usize, A: Allocator> AlignedAllocator<ALIGN, A> {
    #[inline]
    pub const fn new_in(allocator: A) -> Self {
        Self(allocator)
    }
}

/// # Safety: Type is merely a wrapper for aligned allocation of another allocator impl.
unsafe impl<const ALIGN: usize, A: Allocator> Allocator for AlignedAllocator<ALIGN, A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match layout.align_to(ALIGN) {
            Ok(layout) => self.0.allocate(layout),
            Err(_) => Err(AllocError),
        }
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match layout.align_to(ALIGN) {
            Ok(layout) => self.0.allocate_zeroed(layout),
            Err(_) => Err(AllocError),
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        match layout.align_to(ALIGN) {
            // ### Safety: This function shares the same invariants as `GlobalAllocator::deallocate`.
            Ok(layout) => unsafe { self.0.deallocate(ptr, layout) },
            Err(_) => unimplemented!(),
        }
    }
}

pub unsafe fn catch_read(ptr: NonNull<[u8]>) -> Result<TryBox<[u8]>, Exception> {
    let mem_range = ptr.as_uninit_slice().as_ptr_range();
    let aligned_start = libsys::align_down(mem_range.start.addr(), page_size());
    let mem_end = mem_range.end.addr();

    let mut copied_mem = TryBox::new_slice(ptr.len(), 0u8).unwrap();
    for (offset, page_addr) in (aligned_start..mem_end).enumerate().step_by(page_size().get()) {
        let ptr_addr = core::cmp::max(mem_range.start.addr(), page_addr);
        let ptr_len = core::cmp::min(mem_end.saturating_sub(ptr_addr), page_size().get());

        // Safety: Box slice and this iterator are bound by the ptr len.
        let to_ptr = unsafe { (&mut copied_mem).as_mut_ptr().add(offset) };
        // Safety: Copy is only invalid if the caller provided an invalid pointer.
        do_catch(|| unsafe {
            core::ptr::copy_nonoverlapping(ptr_addr as *mut u8, to_ptr, ptr_len);
        })?;
    }

    Ok(copied_mem)
}

// TODO TryString
pub unsafe fn catch_read_str<'a>(mut read_ptr: NonNull<u8>) -> Result<String, Exception> {
    let mut strlen = 0;
    'y: loop {
        let read_len = read_ptr.as_ptr().align_offset(page_size().get());
        read_ptr = NonNull::new(unsafe { read_ptr.as_ptr().add(page_size().get() - read_len) }).unwrap();

        for byte in catch_read(NonNull::slice_from_raw_parts(read_ptr, read_len))?.into_iter() {
            if byte.ne(&b'\0') {
                strlen += 1;
            } else {
                break 'y;
            }
        }
    }

    Ok(String::from_utf8_lossy(core::slice::from_raw_parts(read_ptr.as_ptr(), strlen)).into_owned())
}
