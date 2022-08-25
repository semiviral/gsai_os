use crate::{
    memory::RootPageTable,
    scheduling::{Scheduler, Task, TaskPriority},
};
use core::sync::atomic::{AtomicUsize, Ordering};
use libkernel::{Address, Virtual};

#[repr(C, align(0x1000))]
pub(crate) struct LocalState {
    magic: u64,
    core_id: u32,
    timer: alloc::boxed::Box<dyn crate::time::timer::Timer>,
    scheduler: Scheduler,
    default_task: Task,
    cur_task: Option<Task>,
}

impl LocalState {
    const MAGIC: u64 = 0x1234_B33F_D3AD_C0DE;

    fn is_valid_magic(&self) -> bool {
        self.magic == LocalState::MAGIC
    }
}

static LOCAL_STATES_BASE: AtomicUsize = AtomicUsize::new(0);

/// Returns the pointer to the local state structure.
#[inline]
fn get_local_state() -> Option<&'static mut LocalState> {
    // TODO read from `IA32_KERNEL_GS_BASE`
    unsafe {
        let local_state_ptr = (LOCAL_STATES_BASE.load(Ordering::Relaxed) as *mut LocalState)
            .add(crate::arch::x64::structures::apic::get_id() as usize);

        if  crate::memory::get_kernel_page_manager().is_mapped(Address::<Virtual>::from_ptr(local_state_ptr)) &&  let Some(local_state) = local_state_ptr.as_mut() && local_state.is_valid_magic()  {
                Some(local_state)
            } else {
            None}
    }
}

/// Initializes the core-local state structure.
///
/// SAFETY: This function invariantly assumes it will only be called once.
pub unsafe fn init(core_id: u32) {
    // TODO write to `IA32_KERNEL_GS_BASE`
    LOCAL_STATES_BASE
        .compare_exchange(
            0,
            // Cosntruct the local state pointer (with slide) via the `Address` struct, to
            // automatically sign extend.
            Address::<Virtual>::new_truncate(
                (
                    (510 * libkernel::memory::PML4_ENTRY_MEM_SIZE)
                    // + (libkernel::rand(0..(u32::MAX as u64)).unwrap_or(0) as usize)
                ) & !0xFFF,
            )
            .as_usize(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .ok();

    trace!("Configuring local state: #{}", core_id);

    // Ensure we load the local state pointer via `cpuid` to avoid using the APIC before it is initialized.
    let local_state_ptr = (LOCAL_STATES_BASE.load(Ordering::Relaxed) as *mut LocalState).add(core_id as usize);

    {
        use libkernel::memory::Page;

        // Map the pages this local state will utilize.
        let frame_manager = crate::memory::get_kernel_frame_manager();
        let page_manager = crate::memory::get_kernel_page_manager();
        let base_page = Page::from_ptr(local_state_ptr);
        let end_page = base_page.forward_checked(core::mem::size_of::<LocalState>() / 0x1000).unwrap();
        (base_page..end_page)
            .for_each(|page| page_manager.auto_map(&page, crate::memory::PageAttributes::RW, frame_manager));
    }

    /* CONFIGURE TIMER */
    #[cfg(target_arch = "x86_64")]
    {
        use crate::arch::x64::structures::apic;
        use crate::interrupts::Vector;

        // TODO abstract this somehow, so we can call e.g. `crate::interrupts::configure_controller();`

        trace!("Configuring local APIC...");
        apic::software_reset();
        apic::set_timer_divisor(apic::TimerDivisor::Div1);
        apic::get_timer().set_vector(Vector::Timer as u8).set_masked(false);
        apic::get_error().set_vector(Vector::Error as u8).set_masked(false);
        apic::get_performance().set_vector(Vector::Performance as u8);
        apic::get_thermal_sensor().set_vector(Vector::Thermal as u8);
        // LINT0&1 should be configured by the APIC reset.
    }

    // TODO configure RISC-V ACLINT

    // Ensure interrupts are enabled after APIC is reset.
    crate::interrupts::enable();

    trace!("Writing local state struct out to memory.");
    local_state_ptr.write(LocalState {
        magic: LocalState::MAGIC,
        core_id,
        timer: crate::time::timer::configure_new_timer(1000),
        scheduler: Scheduler::new(false),
        default_task: Task::new(
            TaskPriority::new(1).unwrap(),
            crate::interrupts::wait_loop,
            &crate::scheduling::TaskStackOption::Auto,
            {
                #[cfg(target_arch = "x86_64")]
                {
                    use crate::arch::x64;

                    (
                        x64::cpu::GeneralContext::empty(),
                        x64::cpu::SpecialContext {
                            cs: x64::structures::gdt::KCODE_SELECTOR.get().unwrap().0 as u64,
                            ss: x64::structures::gdt::KDATA_SELECTOR.get().unwrap().0 as u64,
                            flags: x64::registers::RFlags::INTERRUPT_FLAG,
                        },
                    )
                }
            },
            RootPageTable::read(),
        ),
        cur_task: None,
    });

    match get_local_state() {
        Some(local_state) if local_state.is_valid_magic() => {}
        _ => panic!("local state is invalid after write"),
    }

    trace!("Local state structure written to memory and validated.");
}

/// Attempts to schedule the next task in the local task queue.
pub fn schedule_next_task(
    ctrl_flow_context: &mut crate::interrupts::ControlFlowContext,
    arch_context: &mut crate::interrupts::ArchContext,
) {
    const MIN_TIME_SLICE_MS: u16 = 1;
    const PRIO_TIME_SLICE_MS: u16 = 2;

    let local_state = get_local_state().expect("local state is uninitialized");

    // Move the current task, if any, back into the scheduler queue.
    if let Some(mut cur_task) = local_state.cur_task.take() {
        cur_task.ctrl_flow_context = *ctrl_flow_context;
        cur_task.arch_context = *arch_context;
        cur_task.root_page_table_args = RootPageTable::read();

        local_state.scheduler.push_task(cur_task);
    }

    // Take all tasks from the global queue. Every core will be doing this, so we'll load
    // balance the tasks later.
    // while let Some(task) = unsafe { crate::scheduling::GLOBAL_TASK_QUEUE.pop() } {
    //     local_state.scheduler.push_task(task);
    // }

    // {
    //     let active_cpus_list = ACTIVE_CPUS_LIST.read();

    //     for local_state_index in active_cpus_list.iter() {
    //         let other_ptr = unsafe {
    //             (LOCAL_STATES_BASE.load(Ordering::Relaxed) as *mut LocalState).add(*local_state_index as usize)
    //         };

    //         let other = unsafe { other_ptr.as_mut().unwrap() };
    //         let other_avg_prio = other.scheduler.get_avg_prio();
    //         let self_avg_prio = local_state.scheduler.get_avg_prio();
    //         let avg_prio_diff = self_avg_prio.abs_diff(other_avg_prio);
    //     }
    // }

    // load balance tasks
    // {
    //     let rand_index = libkernel::rand(0..ACTIVE_CPUS.load(Ordering::Relaxed)).expect(
    //         "hardware random number generation must be supported for load-balanced scheduling",
    //     ) as usize;
    //     crate::print!(
    //         "rand {:?} {}",
    //         0..ACTIVE_CPUS.load(Ordering::Relaxed),
    //         rand_index
    //     );

    //     let other_ptr = unsafe {
    //         (LOCAL_STATES_BASE.load(Ordering::Relaxed) as *mut LocalState).add(rand_index)
    //     };

    //     if crate::memory::get_kernel_page_manager()
    //         .unwrap()
    //         .is_mapped(Address::<Virtual>::from_ptr(other_ptr))
    //     {
    //         crate::print!("mapped");

    //         let other = unsafe { other_ptr.as_mut().unwrap() };

    //         let self_avg_prio = local_state.scheduler.get_avg_prio();
    //         let other_avg_prio = other.scheduler.get_avg_prio();
    //         const MAX_PRIO_DIFF: u64 = (TaskPriority::MAX + TaskPriority::MIN) as u64;

    //         if self_avg_prio.abs_diff(other_avg_prio) >= MAX_PRIO_DIFF {
    //             while self_avg_prio > other_avg_prio {
    //                 other.scheduler.push_task(
    //                     local_state
    //                         .scheduler
    //                         .pop_task()
    //                         .expect("local scheduler failed to pop task for load balancing"),
    //                 );
    //             }

    //             while self_avg_prio < other_avg_prio {
    //                 local_state.scheduler.push_task(
    //                     other
    //                         .scheduler
    //                         .pop_task()
    //                         .expect("other scheduler failed to pop task for load balancing"),
    //                 );
    //             }
    //         }
    //     }
    // }

    unsafe {
        let next_timer_ms = if let Some(next_task) = local_state.scheduler.pop_task() {
            // Modify interrupt contexts (usually, the registers).
            *ctrl_flow_context = next_task.ctrl_flow_context;
            *arch_context = next_task.arch_context;

            // Set current page tables.
            RootPageTable::write(&next_task.root_page_table_args);

            let next_timer_ms = (next_task.priority().get() as u16) * PRIO_TIME_SLICE_MS;
            local_state.cur_task = Some(next_task);

            next_timer_ms
        } else {
            let default_task = &local_state.default_task;

            // Modify interrupt contexts (usually, the registers).
            *ctrl_flow_context = default_task.ctrl_flow_context;
            *arch_context = default_task.arch_context;

            // Set current page tables.
            RootPageTable::write(&default_task.root_page_table_args);

            MIN_TIME_SLICE_MS
        };

        reload_timer(core::num::NonZeroU16::new(next_timer_ms).unwrap());
    }
}

/// Reloads the local APIC timer with the given millisecond multiplier.
///
/// SAFETY: Caller is expected to only reload timer when appropriate.
unsafe fn reload_timer(freq_multiplier: core::num::NonZeroU16) {
    get_local_state()
        .expect("reload timer called for uninitialized local state")
        .timer
        .set_next_wait(freq_multiplier.get());
}

/// Attempts to begin scheduling tasks on the current thread. If the scheduler has already been
/// enabled, or local state has not been initialized, this function does nothing.
pub fn try_begin_scheduling() {
    if let Some(local_state) = get_local_state() {
        let scheduler = &mut local_state.scheduler;

        if !scheduler.is_enabled() {
            trace!("Enabling kernel scheduler.");
            scheduler.enable();

            unsafe { reload_timer(core::num::NonZeroU16::new_unchecked(1)) };
        }
    }
}

/// Attempts to push a task to the core-local scheduler directly. If the core-local state is not
/// initialized, then the task is returned as an `Err(Task)`.
pub fn try_push_task(task: Task) -> Result<(), Task> {
    match get_local_state() {
        Some(local_state) => {
            local_state.scheduler.push_task(task);
            Ok(())
        }
        None => Err(task),
    }
}
