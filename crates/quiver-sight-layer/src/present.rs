//! `vkQueuePresentKHR`: timestamps, the HUD composite on the target swapchain, the ring
//! records. The call down the chain always happens, outside any panic guard, with the
//! app's semaphores unless our submit consumed them.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use ash::vk;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::layout::{BACKDROP_ERR_DEVICE_LOST, BACKDROP_ERR_INTERNAL, DRAW_ERR_DEVICE_LOST, DRAW_ERR_QUEUE, LAYER_DRAWING, LAYER_HUD_OFF, LAYER_PRESENT_ID};
use quiver_perf::record::LayerPresent;

use crate::compositor::{Drawn, Hud};
use crate::ffi::find_in_chain;
use crate::state::{self, DeviceData, HISTORY, Swapchain, TARGET, lock};
use crate::{draw_enabled, guard, log, telemetry};

/// Swapchains per present the layer records (more are presented but not recorded).
const MAX_SWAPCHAINS: usize = 8;
/// `VK_STRUCTURE_TYPE_PRESENT_ID_2_KHR` (VK_KHR_present_id2; newer than ash's headers).
const STRUCTURE_TYPE_PRESENT_ID_2_KHR: vk::StructureType = vk::StructureType::from_raw(1_000_479_001);

struct Plan {
    n: usize,
    swapchains: [Option<Arc<Mutex<Swapchain>>>; MAX_SWAPCHAINS],
    present_ids: [Option<u64>; MAX_SWAPCHAINS],
    /// Our semaphore, when we submitted: the present waits on it alone.
    semaphore: [vk::Semaphore; 1],
    drawn_on: Option<usize>,
    hud_ns: i64,
    /// Which of the present's swapchains is the HUD's target (Reflex frame counting).
    target: Option<usize>,
}

impl Plan {
    fn new() -> Plan {
        Plan {
            n: 0,
            swapchains: [const { None }; MAX_SWAPCHAINS],
            present_ids: [None; MAX_SWAPCHAINS],
            semaphore: [vk::Semaphore::null()],
            drawn_on: None,
            hud_ns: 0,
            target: None,
        }
    }
}

pub unsafe extern "system" fn queue_present_khr(queue: vk::Queue, info: *const vk::PresentInfoKHR<'_>) -> vk::Result {
    // The lab's GPU governor (labgpu.rs; `quiver lab` games only): hold the present back.
    crate::labgpu::pace();
    let entry = mono_raw_ns();
    let Some(dev) = state::device_by_raw(vk::Handle::as_raw(queue)) else {
        // Not a queue of a device created through this layer: impossible in a valid chain.
        return vk::Result::ERROR_DEVICE_LOST;
    };
    let next = dev.swapchain_fn.queue_present_khr;
    let tel = telemetry::get();
    if tel.is_none() || info.is_null() {
        // SAFETY: the app's arguments, passed down unchanged.
        return unsafe { next(queue, info) };
    }
    // SAFETY: the app's present info is valid for the call.
    let pi = unsafe { &*info };
    let mut plan = Plan::new();
    guard("present", || unsafe { prepare(&dev, queue, pi, entry, &mut plan) });

    let mut patched;
    let to_driver = if plan.semaphore[0] != vk::Semaphore::null() {
        patched = *pi;
        patched.wait_semaphore_count = 1;
        patched.p_wait_semaphores = plan.semaphore.as_ptr();
        &patched as *const vk::PresentInfoKHR<'_>
    } else {
        info
    };
    // SAFETY: the app's arguments, with our semaphore in place of the ones we waited on.
    let result = unsafe { next(queue, to_driver) };
    let ret = mono_raw_ns();
    guard("present records", || unsafe { finish(&dev, pi, &plan, entry, ret, result) });
    result
}

unsafe fn prepare(dev: &DeviceData, queue: vk::Queue, pi: &vk::PresentInfoKHR<'_>, entry: i64, plan: &mut Plan) {
    let count = pi.swapchain_count as usize;
    if count == 0 || pi.p_swapchains.is_null() || pi.p_image_indices.is_null() {
        return;
    }
    let n = count.min(MAX_SWAPCHAINS);
    // SAFETY: the app's arrays have swapchain_count entries.
    let (handles, indices) = unsafe { (std::slice::from_raw_parts(pi.p_swapchains, n), std::slice::from_raw_parts(pi.p_image_indices, n)) };
    // DXVK chains its frame id here (VK_KHR_present_id, or VK_KHR_present_id2's
    // VkPresentId2KHR, the same layout, which DXVK prefers when the driver has it).
    // SAFETY: the app's pNext chain.
    let mut pid = unsafe { find_in_chain(pi.p_next, vk::StructureType::PRESENT_ID_KHR, |_| true) } as *const vk::PresentIdKHR<'_>;
    if pid.is_null() {
        // SAFETY: as above; VkPresentId2KHR is {sType, pNext, swapchainCount, pPresentIds}.
        pid = unsafe { find_in_chain(pi.p_next, STRUCTURE_TYPE_PRESENT_ID_2_KHR, |_| true) } as *const vk::PresentIdKHR<'_>;
    }
    if !pid.is_null() {
        // SAFETY: a PresentIdKHR from the chain.
        let p = unsafe { &*pid };
        if p.swapchain_count as usize == count && !p.p_present_ids.is_null() {
            let ids = unsafe { std::slice::from_raw_parts(p.p_present_ids, n) };
            for (slot, &id) in plan.present_ids.iter_mut().zip(ids) {
                *slot = (id != 0).then_some(id);
            }
        }
    }
    plan.n = n;
    for (slot, &h) in plan.swapchains.iter_mut().zip(handles) {
        *slot = dev.swapchain(h);
    }
    let tel = telemetry::get();
    if let Some(t) = tel
        && plan.present_ids.iter().any(Option::is_some)
    {
        t.flags.fetch_or(LAYER_PRESENT_ID, Ordering::Relaxed);
    }

    let target = TARGET.load(Ordering::Relaxed);
    let Some(i) = plan.swapchains[..n].iter().position(|s| s.as_ref().is_some_and(|s| lock(s).id == target)) else { return };
    plan.target = Some(i);
    let sc = plan.swapchains[i].clone().unwrap();
    let mut s = lock(&sc);
    let mut history = lock(&HISTORY);
    history.push(entry);
    let mut error = s.draw_error;
    let mut drawing = false;
    let mut hud_off = false;
    if draw_enabled() && !dev.lost.load(Ordering::Relaxed) && s.comp.is_some() {
        // SAFETY: the app's wait semaphores, wait_semaphore_count of them.
        let waits = if pi.p_wait_semaphores.is_null() {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(pi.p_wait_semaphores, pi.wait_semaphore_count as usize) }
        };
        let family = dev.queue_family(queue);
        let t0 = mono_raw_ns();
        let r = match family {
            // SAFETY: inside the app's present on `queue`.
            Some(f) => unsafe { s.comp.as_mut().unwrap().draw(dev, queue, f, indices[i], waits, plan.present_ids[i], &history, entry) },
            // A queue we didn't see created: don't draw from it.
            None => Ok(Drawn::Done { semaphore: None, hud: Hud::Nothing(DRAW_ERR_QUEUE), blurred: false }),
        };
        plan.hud_ns = mono_raw_ns() - t0;
        match r {
            Ok(Drawn::Done { semaphore, hud, blurred }) => {
                if let Some(s) = semaphore {
                    plan.semaphore[0] = s;
                }
                match hud {
                    Hud::Drawn => {
                        plan.drawn_on = Some(i);
                        drawing = true;
                        error = 0;
                    }
                    Hud::Off => {
                        hud_off = true;
                        error = 0;
                    }
                    Hud::Nothing(reason) => error = reason,
                }
                if blurred && let Some(t) = tel {
                    t.blur_last_ns.store(entry, Ordering::Relaxed);
                }
            }
            Ok(Drawn::Skipped) => return,
            Err(e) => {
                // Drawing off for this swapchain; resources go with it at destroy.
                log!("compositor disabled on swapchain {}: draw_error {e}", s.id);
                s.draw_error = e;
                error = e;
                if e == DRAW_ERR_DEVICE_LOST {
                    dev.lost.store(true, Ordering::Relaxed);
                }
                // The backdrop blur goes with the compositor.
                if let Some(t) = tel {
                    t.blur_error.store(if e == DRAW_ERR_DEVICE_LOST { BACKDROP_ERR_DEVICE_LOST } else { BACKDROP_ERR_INTERNAL }, Ordering::Relaxed);
                }
                if let Some(c) = s.comp.take() {
                    // Keep the objects alive until the swapchain goes (they may be in flight).
                    s.disabled.push(c);
                }
            }
        }
    }
    if let Some(t) = tel {
        t.draw_error.store(error, Ordering::Relaxed);
        let set = |on: bool, bit: u32| {
            if on {
                t.flags.fetch_or(bit, Ordering::Relaxed);
            } else {
                t.flags.fetch_and(!bit, Ordering::Relaxed);
            }
        };
        set(drawing, LAYER_DRAWING);
        set(hud_off, LAYER_HUD_OFF);
        if drawing {
            t.drawn.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe fn finish(dev: &DeviceData, pi: &vk::PresentInfoKHR<'_>, plan: &Plan, entry: i64, ret: i64, result: vk::Result) {
    let Some(t) = telemetry::get() else { return };
    if result == vk::Result::ERROR_DEVICE_LOST {
        dev.lost.store(true, Ordering::Relaxed);
        t.draw_error.store(DRAW_ERR_DEVICE_LOST, Ordering::Relaxed);
    }
    // SAFETY: the app's arrays (checked non-null in prepare when n > 0).
    let indices = if plan.n > 0 { unsafe { std::slice::from_raw_parts(pi.p_image_indices, plan.n) } } else { &[][..] };
    let results = if plan.n > 0 && !pi.p_results.is_null() { Some(unsafe { std::slice::from_raw_parts(pi.p_results, plan.n) }) } else { None };
    for i in 0..plan.n {
        let Some(sc) = &plan.swapchains[i] else { continue };
        let (id, seq, mode, latency) = {
            let mut s = lock(sc);
            s.seq += 1;
            (s.id, s.seq, s.present_mode, (plan.target == Some(i)).then(|| s.latency.clone()))
        };
        // Reflex frame attribution and counting, on the HUD's swapchain only.
        let app_frame = latency.and_then(|l| l.on_present(ret));
        // The graph's base series: the presents that start an application frame (their
        // entry times, like the presented series').
        if app_frame == Some(true) {
            lock(&HISTORY).push_base(entry);
        }
        let rec = LayerPresent {
            swapchain: id,
            present_id: plan.present_ids[i],
            seq,
            entry_ns: entry,
            return_ns: ret,
            image_index: indices[i],
            present_mode: mode.as_raw() as u32,
            result: results.map_or(result, |r| r[i]).as_raw(),
            hud_drawn: plan.drawn_on == Some(i),
            hud_ns: if plan.drawn_on == Some(i) { plan.hud_ns } else { 0 },
            app_frame,
        };
        t.ring.push(&rec.to_record());
    }
    t.presents.fetch_add(pi.swapchain_count as u64, Ordering::Relaxed);
}
