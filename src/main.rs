#![warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

use std::mem::MaybeUninit;
use std::sync::{Arc, Mutex};
use garnshared::platform_traits::PlatformMutex;
use crate::early_failure::early_failure;
use crate::linux::shm_allocator::ShmSync;
use crate::util::get_optional_env_var;

mod constants;
mod early_failure;
mod util;

cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        mod linux;
        use linux::runtime::Runtime;
        use linux::shm_allocator::ShmAllocator;
    }
}

unsafe impl ShmSync for i32 {}
unsafe impl ShmSync for u64 {}
unsafe impl ShmSync for f32 {}
unsafe impl ShmSync for f64 {}

fn main() {
    let (runtime, error_receiver) = Runtime::new(
        get_optional_env_var(constants::WORKING_DIR_ENV_OPTION).as_deref(), // Custom working directory
    );

    let runtime = match runtime.init() {
        Ok(res) => res,
        Err(e) => early_failure(&e.to_string()),
    };

    let runtime = runtime.listen();

    let mut shm_allocator = ShmAllocator::new().unwrap();
    for i in 0..100 {
        let small_int = i;
        let big_int = i as u64;
        let small_float = i as f32;
        let big_float = i as f64;
        let mut mutex = MaybeUninit::<garnshared::linux::pthread_mutex::PthreadMutex>::uninit();
        unsafe {
            garnshared::linux::pthread_mutex::PthreadMutex::init(&mut mutex as *mut MaybeUninit<garnshared::linux::pthread_mutex::PthreadMutex>);
            let mutex = mutex.assume_init();
            mutex.lock();
            mutex.try_lock();
            mutex.unlock();
        }

        shm_allocator.move_into_shm(format!("si{i}").as_str(), small_int).unwrap();
        shm_allocator.move_into_shm(format!("bi{i}").as_str(), big_int).unwrap();
        shm_allocator.move_into_shm(format!("sf{i}").as_str(), small_float).unwrap();
        shm_allocator.move_into_shm(format!("bf{i}").as_str(), big_float).unwrap();
    }
    for i in 0..100 {
        let small_int = shm_allocator.access_resource::<i32>(format!("si{i}").as_str()).unwrap();
        let big_int = shm_allocator.access_resource::<u64>(format!("bi{i}").as_str()).unwrap();
        let small_float = shm_allocator.access_resource::<f32>(format!("sf{i}").as_str()).unwrap();
        let big_float = shm_allocator.access_resource::<f64>(format!("bf{i}").as_str()).unwrap();

        println!("{} {} {} {}", small_int, big_int, small_float, big_float);
    }
}
