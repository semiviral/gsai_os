pub mod address_space;
pub mod alloc;
pub mod io;
pub mod paging;

use crate::{exceptions::Exception, interrupts::InterruptCell, local::do_catch, memory::address_space::mapper::Mapper};
use ::alloc::string::String;
use core::ptr::NonNull;
use libsys::{page_size, table_index_size, Address, Frame, Page, Virtual};
use spin::{Mutex, Once};
use try_alloc::boxed::TryBox;

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hhdm(Address<Page>);

impl Hhdm {
    fn get() -> Self {
        static HHDM_ADDRESS: Once<Hhdm> = Once::new();

        *HHDM_ADDRESS.call_once(|| {
            static LIMINE_HHDM: limine::HhdmRequest = limine::HhdmRequest::new(crate::boot::LIMINE_REV);

            let address = Address::new(
                LIMINE_HHDM
                    .get_response()
                    .expect("bootloader provided no higher-half direct mapping")
                    .offset()
                    .try_into()
                    .unwrap(),
            )
            .unwrap();

            Self(address)
        })
    }

    #[inline]
    pub fn page() -> Address<Page> {
        Self::get().0
    }

    #[inline]
    pub fn address() -> Address<Virtual> {
        Self::get().0.get()
    }

    #[inline]
    pub fn ptr() -> *mut u8 {
        Self::address().as_ptr()
    }

    #[inline]
    pub fn offset(frame: Address<Frame>) -> Option<Address<Page>> {
        Address::new(Self::address().get() + frame.get().get())
    }
}

pub struct Stack<const SIZE: usize>([u8; SIZE]);

impl<const SIZE: usize> Stack<SIZE> {
    #[inline]
    pub const fn new() -> Self {
        Self([0u8; SIZE])
    }

    pub fn top(&self) -> Address<Virtual> {
        // Safety: Pointer is valid for the length of the slice.
        Address::from_ptr(unsafe { self.0.as_ptr().add(self.0.len()).cast_mut() })
    }
}

impl<const SIZE: usize> core::ops::Deref for Stack<SIZE> {
    type Target = [u8; SIZE];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn with_kmapper<T>(func: impl FnOnce(&mut Mapper) -> T) -> T {
    static KERNEL_MAPPER: Once<InterruptCell<Mutex<Mapper>>> = Once::new();

    KERNEL_MAPPER
        .call_once(|| {
            debug!("Creating kernel-space address mapper.");

            InterruptCell::new(Mutex::new(Mapper::new(paging::PageDepth::current()).unwrap()))
        })
        .with(|mapper| {
            let mut mapper = mapper.lock();
            func(&mut mapper)
        })
}

pub fn copy_kernel_page_table() -> alloc::pmm::Result<Address<Frame>> {
    let table_frame = alloc::pmm::PMM.next_frame()?;

    // Safety: Frame is provided by allocator, and so guaranteed to be within the HHDM, and is frame-sized.
    let new_table = unsafe {
        core::slice::from_raw_parts_mut(
            Hhdm::offset(table_frame).unwrap().as_ptr().cast::<paging::TableEntry>(),
            table_index_size().get(),
        )
    };
    new_table.fill(paging::TableEntry::empty());
    with_kmapper(|kmapper| new_table.copy_from_slice(kmapper.view_page_table()));

    Ok(table_frame)
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

    /// Safety
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

#[allow(clippy::module_name_repetitions)]
pub unsafe fn out_of_memory() -> ! {
    panic!("Kernel ran out of memory during initialization.")
}

pub unsafe fn catch_read(ptr: NonNull<[u8]>) -> Result<TryBox<[u8]>, Exception> {
    let mem_range = ptr.as_uninit_slice().as_ptr_range();
    let aligned_start = libsys::align_down(mem_range.start.addr(), libsys::page_shift());
    let mem_end = mem_range.end.addr();

    let mut copied_mem = TryBox::new_slice(ptr.len(), 0u8).unwrap();
    for (offset, page_addr) in (aligned_start..mem_end).enumerate().step_by(page_size()) {
        let ptr_addr = core::cmp::max(mem_range.start.addr(), page_addr);
        let ptr_len = core::cmp::min(mem_end.saturating_sub(ptr_addr), page_size());

        // Safety: Box slice and this iterator are bound by the ptr len.
        let to_ptr = unsafe { copied_mem.as_mut_ptr().add(offset) };
        // Safety: Copy is only invalid if the caller provided an invalid pointer.
        do_catch(|| unsafe {
            core::ptr::copy_nonoverlapping(ptr_addr as *mut u8, to_ptr, ptr_len);
        })?;
    }

    Ok(copied_mem)
}

// TODO TryString
pub unsafe fn catch_read_str(mut read_ptr: NonNull<u8>) -> Result<String, Exception> {
    let mut strlen = 0;
    'y: loop {
        let read_len = read_ptr.as_ptr().align_offset(page_size());
        read_ptr = NonNull::new(
            // Safety: This pointer isn't used without first being validated.
            unsafe { read_ptr.as_ptr().add(page_size() - read_len) },
        )
        .unwrap();

        for byte in catch_read(NonNull::slice_from_raw_parts(read_ptr, read_len))?.iter() {
            if byte.ne(&b'\0') {
                strlen += 1;
            } else {
                break 'y;
            }
        }
    }

    Ok(String::from_utf8_lossy(core::slice::from_raw_parts(read_ptr.as_ptr(), strlen)).into_owned())
}
