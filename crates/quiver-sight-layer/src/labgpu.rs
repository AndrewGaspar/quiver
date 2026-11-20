//! The lab's GPU limit (design/LAB.md §15), only in `quiver lab` games: `QUIVER_LAB_GPU`
//! names the instance's `gpu.ctl` page ([`quiver_perf::labgpu`]). Without it nothing here
//! does anything, and a game of the person's never has it.
//!
//! - **Priority**: devices are created with every queue at `VK_QUEUE_GLOBAL_PRIORITY_LOW`
//!   (`VK_KHR_global_priority`, else `VK_EXT_global_priority`), when the supervisor asks and
//!   the driver has it. A driver that refuses gets the app's own create info instead.
//! - **Pacing**: each present waits until the page's minimum interval since the last one has
//!   passed (the supervisor's governor sets it from the instance's measured GPU share). It
//!   works without a session dir (`QUIVER_PERF_DIR`): the rest of the layer passes through.
//!
//! The present path sleeps here, on purpose: holding presents back is how the game's GPU work
//! is held back (its next frame waits for this one). Only while the page says so.

use std::ffi::{CStr, c_char};
use std::sync::{Mutex, OnceLock};

use ash::vk;
use quiver_perf::clock::mono_raw_ns;
pub use quiver_perf::labgpu::Priority;
use quiver_perf::labgpu::{self, Ctl, PRIORITY_LOW};
use quiver_perf::shm::Mapping;

use crate::ffi::find_in_chain;
use crate::log;

fn ctl() -> Option<&'static Ctl<Mapping>> {
    static C: OnceLock<Option<Ctl<Mapping>>> = OnceLock::new();
    C.get_or_init(|| {
        let p = std::env::var_os(labgpu::ENV).filter(|p| !p.is_empty())?;
        match Mapping::open_write(std::path::Path::new(&p), labgpu::LEN, labgpu::LEN).ok().and_then(Ctl::attach) {
            Some(c) => {
                log!("lab GPU limit page {} (priority {}, interval {} ns)", p.to_string_lossy(), c.priority_wanted(), c.interval_ns());
                Some(c)
            }
            None => {
                log!("lab GPU limit page {} isn't usable; no limit in this process", p.to_string_lossy());
                None
            }
        }
    })
    .as_ref()
}

/// A device create info with every queue at low global priority, and what it owns.
pub struct Lowered {
    _queues: Vec<vk::DeviceQueueCreateInfo<'static>>,
    _prio: Vec<Box<vk::DeviceQueueGlobalPriorityCreateInfoKHR<'static>>>,
    _exts: Vec<*const c_char>,
    info: vk::DeviceCreateInfo<'static>,
    pub app_chose: bool,
}

impl Lowered {
    pub fn info(&self) -> *const vk::DeviceCreateInfo<'static> {
        &self.info
    }
}

/// The page wants low priority for new devices.
pub fn wants_low() -> bool {
    ctl().is_some_and(|c| c.priority_wanted() == PRIORITY_LOW)
}

/// Record what the device got.
pub fn note(p: Priority) {
    if let Some(c) = ctl() {
        c.set_priority(p);
    }
    log!("lab GPU limit: {}", p.describe());
}

/// `ci` with low priority on every queue the app didn't give one, and the extension enabled;
/// `Err` when the driver has neither extension.
///
/// # Safety
/// `ci` is the app's valid create info; `instance` owns `physical`.
pub unsafe fn lower(instance: &ash::Instance, physical: vk::PhysicalDevice, ci: *const vk::DeviceCreateInfo<'_>) -> Result<Lowered, Priority> {
    // SAFETY: the instance's own function on its physical device.
    let props = unsafe { instance.enumerate_device_extension_properties(physical) }.unwrap_or_default();
    let has = |n: &CStr| props.iter().any(|p| p.extension_name_as_c_str().is_ok_and(|e| e == n));
    let ext = if has(ash::khr::global_priority::NAME) {
        ash::khr::global_priority::NAME
    } else if has(ash::ext::global_priority::NAME) {
        ash::ext::global_priority::NAME
    } else {
        return Err(Priority::Unsupported);
    };
    // The levels each queue family offers, when the driver can say (KHR, or the EXT query):
    // NVIDIA's 620 driver lists only MEDIUM, and then asking for LOW changes nothing.
    let mut offered: Option<Vec<bool>> = None;
    if has(ash::khr::global_priority::NAME) || has(ash::ext::global_priority_query::NAME) {
        // SAFETY: the instance's own functions on its physical device; the out structs live
        // across the call.
        let o = unsafe {
            let n = instance.get_physical_device_queue_family_properties2_len(physical);
            let mut gp = vec![vk::QueueFamilyGlobalPriorityPropertiesKHR::default(); n];
            {
                let mut qf: Vec<vk::QueueFamilyProperties2<'_>> = gp.iter_mut().map(|g| vk::QueueFamilyProperties2::default().push_next(g)).collect();
                instance.get_physical_device_queue_family_properties2(physical, &mut qf);
            }
            gp.iter().map(|g| g.priorities[..(g.priority_count as usize).min(g.priorities.len())].contains(&vk::QueueGlobalPriorityKHR::LOW)).collect::<Vec<bool>>()
        };
        if !o.is_empty() && !o.iter().any(|&x| x) {
            return Err(Priority::NotOffered);
        }
        offered = Some(o).filter(|o| !o.is_empty());
    }
    // SAFETY: the app's create info and its arrays, valid for the call.
    let ci = unsafe { &*ci };
    let n = ci.queue_create_info_count as usize;
    let src: &[vk::DeviceQueueCreateInfo<'_>] = if n == 0 || ci.p_queue_create_infos.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(ci.p_queue_create_infos, n) } };
    let mut prio = Vec::with_capacity(n);
    let mut queues = Vec::with_capacity(n);
    let mut app_chose = false;
    for q in src {
        // SAFETY: the app's pNext chain.
        let own = unsafe { find_in_chain(q.p_next, vk::StructureType::DEVICE_QUEUE_GLOBAL_PRIORITY_CREATE_INFO_KHR, |_| true) };
        // SAFETY: the same struct with its lifetime erased; it lives in the app's memory for the call.
        let mut q2: vk::DeviceQueueCreateInfo<'static> = unsafe { std::mem::transmute(*q) };
        // A family that doesn't offer LOW keeps the default.
        let family_ok = offered.as_ref().is_none_or(|o| o.get(q.queue_family_index as usize).copied().unwrap_or(true));
        if own.is_null() && family_ok {
            let mut p = Box::new(vk::DeviceQueueGlobalPriorityCreateInfoKHR::default().global_priority(vk::QueueGlobalPriorityKHR::LOW));
            p.p_next = q.p_next;
            q2.p_next = (&*p as *const vk::DeviceQueueGlobalPriorityCreateInfoKHR<'_>).cast();
            prio.push(p);
        } else if !own.is_null() {
            app_chose = true;
        }
        queues.push(q2);
    }
    let mut exts: Vec<*const c_char> = if ci.enabled_extension_count == 0 || ci.pp_enabled_extension_names.is_null() {
        Vec::new()
    } else {
        // SAFETY: the app's array.
        unsafe { std::slice::from_raw_parts(ci.pp_enabled_extension_names, ci.enabled_extension_count as usize) }.to_vec()
    };
    // SAFETY: NUL-terminated names from the app.
    let enabled = exts.iter().any(|&e| !e.is_null() && unsafe { CStr::from_ptr(e) } == ext);
    if !enabled {
        exts.push(ext.as_ptr());
    }
    // SAFETY: as above.
    let mut info: vk::DeviceCreateInfo<'static> = unsafe { std::mem::transmute(*ci) };
    info.queue_create_info_count = queues.len() as u32;
    info.p_queue_create_infos = if queues.is_empty() { std::ptr::null() } else { queues.as_ptr() };
    info.enabled_extension_count = exts.len() as u32;
    info.pp_enabled_extension_names = exts.as_ptr();
    Ok(Lowered { _queues: queues, _prio: prio, _exts: exts, info, app_chose })
}

/// Before a present: wait until the page's interval since the last present has passed.
pub fn pace() {
    let Some(c) = ctl() else { return };
    static LAST: Mutex<i64> = Mutex::new(0);
    let interval = c.interval_ns();
    let now = mono_raw_ns();
    let deadline = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let d = labgpu::next_deadline(*last, now, interval);
        *last = d;
        d
    };
    let mut held = 0;
    if deadline > now {
        // At most a second per present, whatever the page says.
        let wait = ((deadline - now) as u64).min(1_000_000_000);
        std::thread::sleep(std::time::Duration::from_nanos(wait));
        held = (mono_raw_ns() - now).max(0) as u64;
    }
    c.presented(std::process::id(), held);
}
