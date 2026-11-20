//! The lab's GPU limit in a real Vulkan app (tests/harness, design/LAB.md §15): with
//! `QUIVER_LAB_GPU` naming a `gpu.ctl` page, the layer creates the device with low global
//! queue priority and holds presents to the page's interval; with no session dir (a lab game
//! without Sight) the rest of the layer passes through. Validation runs below the layer when a
//! validation layer is on this machine.
//!
//! Needs `VK_EXT_headless_surface` (Mesa: RADV, ANV), so it's opt-in:
//! `cargo test -p quiver-sight-layer --test vulkan_labgpu -- --ignored --nocapture`
//! (`QUIVER_TEST_VK_DEVICE=<name substring>` picks the device).

mod harness;

use std::time::Instant;

use harness::*;
use quiver_perf::labgpu::{self, Ctl, Priority};
use quiver_perf::shm::Mapping;

#[test]
#[ignore = "needs a GPU with VK_EXT_headless_surface; see the module docs"]
fn low_priority_and_paced_presents() {
    let scratch = tempfile::tempdir().unwrap();
    write_manifest(scratch.path(), &layer_so());
    let validation = if std::env::var("QUIVER_TEST_VALIDATION").as_deref() == Ok("0") { None } else { validation_dir() };
    let page = scratch.path().join(labgpu::FILE);
    let ctl = Ctl::init(Mapping::create(&page, labgpu::LEN).unwrap(), labgpu::PRIORITY_LOW).unwrap();
    // SAFETY: the only test in this binary; nothing reads the environment concurrently.
    unsafe {
        for (k, v) in layer_env(scratch.path(), validation.as_deref()) {
            std::env::set_var(k, v);
        }
        std::env::remove_var("QUIVER_PERF_DIR");
        std::env::set_var("QUIVER_SIGHT_LOG", "1");
        std::env::set_var(labgpu::ENV, &page);
    }
    // Free: no interval.
    let t = Instant::now();
    let free = run(Opts { frames: 60, fps: 1000.0, readback: false, debug_utils: validation.is_some(), ..Opts::default() });
    let free_s = t.elapsed().as_secs_f64();
    let s = ctl.snapshot();
    eprintln!("device {}: {} presents in {free_s:.3}s free; {:?}", free.device_name, s.presents, s.priority);
    for m in &free.messages {
        eprintln!("{m}");
    }
    assert!(free.messages.is_empty(), "validation reported {} messages", free.messages.len());
    assert_eq!(s.presents, 60);
    assert_eq!(s.held, 0, "no interval, nothing held");
    assert!(matches!(s.priority, Priority::Low | Priority::Unsupported), "{:?}", s.priority);
    assert!(s.devices >= 1);

    // Held to 20 ms a frame: 60 presents take at least 59 intervals.
    ctl.set_interval_ns(20_000_000);
    let t = Instant::now();
    let paced = run(Opts { frames: 60, fps: 1000.0, readback: false, debug_utils: validation.is_some(), ..Opts::default() });
    let paced_s = t.elapsed().as_secs_f64();
    let s2 = ctl.snapshot();
    let gaps: Vec<u64> = paced.present_ns.clone();
    eprintln!("paced: {} presents in {paced_s:.3}s, held {} for {:.3}s; present call median {} us", s2.presents - s.presents, s2.held, s2.held_ns as f64 / 1e9, median(&gaps) / 1000);
    assert_eq!(s2.presents - s.presents, 60);
    assert!(paced_s >= 59.0 * 0.020 * 0.98, "{paced_s}s");
    assert!(s2.held >= 50, "most presents were held back ({})", s2.held);
    assert!(paced.messages.is_empty(), "validation: {:?}", paced.messages);
    // The device was made at low priority (or the driver can't, and says so).
    eprintln!("priority: {}", s2.priority.describe());
}

/// What each GPU's driver does with a device whose queue asks for low global priority (no
/// layer involved): `cargo test -p quiver-sight-layer --test vulkan_labgpu driver -- --ignored
/// --nocapture`. Prints, per device: the extensions, the priorities its queue families allow,
/// and the result of creating a device at LOW and at the default.
#[test]
#[ignore = "needs a GPU; prints what the drivers do"]
fn driver_low_priority() {
    use ash::vk;
    // SAFETY: test code; every handle is destroyed.
    unsafe {
        let entry = ash::Entry::load().unwrap();
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
        let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None).unwrap();
        for pd in instance.enumerate_physical_devices().unwrap() {
            let props = instance.get_physical_device_properties(pd);
            let name = props.device_name_as_c_str().unwrap().to_string_lossy().into_owned();
            let exts = instance.enumerate_device_extension_properties(pd).unwrap();
            let has = |n: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str().is_ok_and(|x| x == n));
            let (khr, ext, query) = (has(ash::khr::global_priority::NAME), has(ash::ext::global_priority::NAME), has(ash::ext::global_priority_query::NAME));
            let n = instance.get_physical_device_queue_family_properties2_len(pd);
            let mut gp = vec![vk::QueueFamilyGlobalPriorityPropertiesKHR::default(); n];
            {
                let mut qf: Vec<vk::QueueFamilyProperties2> = gp.iter_mut().map(|g| vk::QueueFamilyProperties2::default().push_next(g)).collect();
                if khr || query {
                    instance.get_physical_device_queue_family_properties2(pd, &mut qf);
                }
            }
            let allowed: Vec<Vec<vk::QueueGlobalPriorityKHR>> = gp.iter().map(|g| g.priorities[..g.priority_count as usize].to_vec()).collect();
            eprintln!("{name}: KHR {khr} EXT {ext} query {query}; family priorities {allowed:?}");
            let try_create = |prio: Option<vk::QueueGlobalPriorityKHR>| {
                let pri = [1.0f32];
                let mut gpci = vk::DeviceQueueGlobalPriorityCreateInfoKHR::default().global_priority(prio.unwrap_or(vk::QueueGlobalPriorityKHR::MEDIUM));
                let mut q = vk::DeviceQueueCreateInfo::default().queue_family_index(0).queue_priorities(&pri);
                if prio.is_some() {
                    q = q.push_next(&mut gpci);
                }
                let qs = [q];
                let names = [if khr { ash::khr::global_priority::NAME.as_ptr() } else { ash::ext::global_priority::NAME.as_ptr() }];
                let mut ci = vk::DeviceCreateInfo::default().queue_create_infos(&qs);
                if prio.is_some() && (khr || ext) {
                    ci = ci.enabled_extension_names(&names);
                }
                match instance.create_device(pd, &ci, None) {
                    Ok(d) => {
                        d.destroy_device(None);
                        "ok".to_owned()
                    }
                    Err(e) => format!("{e:?}"),
                }
            };
            eprintln!("  default: {}", try_create(None));
            if khr || ext {
                for p in [vk::QueueGlobalPriorityKHR::LOW, vk::QueueGlobalPriorityKHR::MEDIUM] {
                    eprintln!("  {p:?}: {}", try_create(Some(p)));
                }
            }
        }
        instance.destroy_instance(None);
    }
}
